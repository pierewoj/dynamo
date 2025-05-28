# SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# `dynamo-run out=sglang` runs this script
# Can also be used standalone: `python3 sglang_inc.py` - lots of optional cmd line params

import argparse
import asyncio
import logging
import sys
from typing import Optional

import sglang
import uvloop
from sglang.srt.server_args import ServerArgs

from dynamo.llm import ModelType, register_llm
from dynamo.runtime import DistributedRuntime, dynamo_worker

# Only used if you run it manually from the command line
DEFAULT_ENDPOINT = "dyn://dynamo.backend.generate"
DEFAULT_MODEL = "Qwen/Qwen3-0.6B"

logging.basicConfig(level=logging.DEBUG)


class Config:
    """Command line parameters or defaults"""

    namespace: str
    component: str
    endpoint: str
    model_path: str
    model_name: Optional[str]
    base_gpu_id: int
    tensor_parallel_size: int
    kv_block_size: int
    context_length: int
    nnodes: int
    node_rank: int
    dist_init_addr: str
    extra_engine_args: str


class RequestHandler:
    """
    Request handler for the generate endpoint
    """

    def __init__(self, engine):
        self.engine_client = engine

    async def generate(self, request):
        sampling_params = {}
        if request["sampling_options"]["temperature"] is not None:
            sampling_params["temperature"] = request["sampling_options"]["temperature"]
        sampling_params = {
            # sglang defaults this to 128
            "max_new_tokens": request["stop_conditions"]["max_tokens"],
        }
        num_output_tokens_so_far = 0
        gen = await self.engine_client.async_generate(
            input_ids=request["token_ids"], sampling_params=sampling_params, stream=True
        )
        async for res in gen:
            # res is a dict

            finish_reason = res["meta_info"]["finish_reason"]
            if finish_reason:
                # Don't forward the stop token
                out = {"token_ids": [], "finish_reason": finish_reason["type"]}
            else:
                next_total_toks = len(res["output_ids"])
                out = {"token_ids": res["output_ids"][num_output_tokens_so_far:]}
            yield out
            num_output_tokens_so_far = next_total_toks


@dynamo_worker(static=False)
async def worker(runtime: DistributedRuntime):
    await init(runtime, cmd_line_args())


async def init(runtime: DistributedRuntime, config: Config):
    """
    Instantiate and serve
    """

    arg_map = {
        "model_path": config.model_path,
        "skip_tokenizer_init": True,
        "tp_size": config.tensor_parallel_size,
        "base_gpu_id": config.base_gpu_id,
    }

    if config.kv_block_size:
        arg_map["page_size"] = config.kv_block_size

    if config.context_length:
        arg_map["context_length"] = config.context_length

    if config.dist_init_addr != "":
        arg_map["trust_remote_code"] = True
        arg_map["nnodes"] = config.nnodes
        arg_map["dist_init_addr"] = config.dist_init_addr
        # In practice this is always 0 because Dynamo only manages the leader
        arg_map["node_rank"] = config.node_rank

    if config.extra_engine_args != "":
        json_map = {}
        # extra_engine_args is a filename
        try:
            with open(config.extra_engine_args) as f:
                json_map = json.load(f)
        except FileNotFoundError:
            logging.error(f"File {config.extra_engine_args} not found.")
        except json.JSONDecodeError as e:
            logging.error(f"Invalid JSON in {config.extra_engine_args}: {e}")
        logging.debug(f"Adding extra engine arguments: {json_map}")
        arg_map = {**arg_map, **json_map}  # json_map gets precedence

    # TODO fetch default SamplingParams from generation_config.json

    engine_args = ServerArgs(**arg_map)
    engine_client = sglang.Engine(server_args=engine_args)

    component = runtime.namespace(config.namespace).component(config.component)
    await component.create_service()

    endpoint = component.endpoint(config.endpoint)
    await register_llm(
        ModelType.Backend, endpoint, config.model_path, config.model_name
    )

    # the server will gracefully shutdown (i.e., keep opened TCP streams finishes)
    # after the lease is revoked
    await endpoint.serve_endpoint(RequestHandler(engine_client).generate)


def cmd_line_args():
    parser = argparse.ArgumentParser(
        description="SGLang server integrated with Dynamo LLM."
    )
    parser.add_argument(
        "--endpoint",
        type=str,
        default=DEFAULT_ENDPOINT,
        help=f"Dynamo endpoint string in 'dyn://namespace.component.endpoint' format. Default: {DEFAULT_ENDPOINT}",
    )
    parser.add_argument(
        "--model-path",
        type=str,
        default=DEFAULT_MODEL,
        help=f"Path to disk model or HuggingFace model identifier to load. Default: {DEFAULT_MODEL}",
    )
    parser.add_argument(
        "--model-name",
        type=str,
        default="",
        help="Name to serve the model under. Defaults to deriving it from model path.",
    )
    parser.add_argument(
        "--base-gpu-id",
        type=int,
        default=0,
        help="The base GPU ID to start allocating GPUs from. Useful when running multiple instances on the same machine.",
    )
    parser.add_argument(
        "--tensor-parallel-size", type=int, default=1, help="Number of GPUs to use."
    )
    parser.add_argument(
        "--kv-block-size", type=int, default=16, help="Size of a KV cache block."
    )
    parser.add_argument(
        "--context-length",
        type=int,
        default=None,
        help="Max model context length. Defaults to models max, usually model_max_length from tokenizer_config.json. Reducing this reduces VRAM requirements.",
    )
    parser.add_argument(
        "--nnodes", type=int, default=1, help="The number of machines SGLang will use"
    )
    parser.add_argument(
        "--node-rank",
        type=int,
        default=0,
        help="Unique number for each node. 0 for the leader.",
    )
    parser.add_argument(
        "--dist-init-addr",
        type=str,
        default="",
        help="Host address (e.g., `192.168.0.2:25000`) of the node with rank 0",
    )
    parser.add_argument(
        "--extra-engine-args",
        type=str,
        default="",
        help="Path to a JSON file containing additional keyword arguments to pass to the SGLang Engine.",
    )
    args = parser.parse_args()

    config = Config()
    config.model_path = args.model_path
    if args.model_name:
        config.model_name = args.model_name
    else:
        # This becomes an `Option` on the Rust side
        config.model_name = None

    endpoint_str = args.endpoint.replace("dyn://", "", 1)
    endpoint_parts = endpoint_str.split(".")
    if len(endpoint_parts) != 3:
        logging.error(
            f"Invalid endpoint format: '{args.endpoint}'. Expected 'dyn://namespace.component.endpoint' or 'namespace.component.endpoint'."
        )
        sys.exit(1)

    parsed_namespace, parsed_component_name, parsed_endpoint_name = endpoint_parts

    config.namespace = parsed_namespace
    config.component = parsed_component_name
    config.endpoint = parsed_endpoint_name
    config.base_gpu_id = args.base_gpu_id
    config.tensor_parallel_size = args.tensor_parallel_size
    config.kv_block_size = args.kv_block_size
    config.context_length = args.context_length
    config.nnodes = args.nnodes
    config.node_rank = args.node_rank
    config.dist_init_addr = args.dist_init_addr
    config.extra_engine_args = args.extra_engine_args

    return config


if __name__ == "__main__":
    uvloop.install()
    asyncio.run(worker())
