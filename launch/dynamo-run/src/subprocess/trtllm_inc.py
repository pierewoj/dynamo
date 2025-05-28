# SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# TODO:
# - Support disaggregated serving
# - Update examples to use this engine.
#
# `dynamo-run out=trtllm` runs this script
# Can be used standalone: `python3 trtllm_inc.py` - lots of optional cmd line params

import argparse
import asyncio
import logging
import sys
import warnings
from typing import Optional

import uvloop

# Import TRTLLM and related modules
from tensorrt_llm import SamplingParams
from tensorrt_llm.llmapi.llm_utils import update_llm_args_with_extra_options
from tensorrt_llm.llmapi.tokenizer import tokenizer_factory

from dynamo.llm import (
    ModelType,
    get_tensorrtllm_engine,
    get_tensorrtllm_publisher,
    register_llm,
)
from dynamo.runtime import DistributedRuntime, dynamo_worker

# Only used if you run it manually from the command line
DEFAULT_ENDPOINT = "dyn://dynamo.backend.generate"
# Qwen/Qwen3-0.6B is not supported by TRTLLM yet.
DEFAULT_MODEL = "TinyLlama/TinyLlama-1.1B-Chat-v1.0"

# Default buffer size for kv cache events.
DEFAULT_KV_EVENT_BUFFER_MAX_SIZE = 1024

logging.basicConfig(level=logging.DEBUG)


class Config:
    """Command line parameters or defaults"""

    namespace: str
    component: str
    endpoint: str
    model_path: str
    model_name: Optional[str] = None
    tensor_parallel_size: int
    kv_block_size: int
    extra_engine_args: str
    publish_events_and_metrics: bool


class RequestHandler:
    """
    Request handler for the generate endpoint
    """

    def __init__(self, component, engine, default_sampling_params, publishers):
        self.engine = engine
        self.component = component
        self.default_sampling_params = default_sampling_params
        self.publishers = publishers
        self.first_generation = True

    async def generate(self, request):
        # Check if there is an error in the publishers error queue
        publishers_error = (
            self.publishers.check_error_queue() if self.publishers else None
        )
        if publishers_error:
            raise publishers_error

        inputs = request["token_ids"]

        sampling_params = self.default_sampling_params
        for key, value in request["sampling_options"].items():
            if not value:
                continue
            if hasattr(sampling_params, key):
                setattr(sampling_params, key, value)

        max_tokens = request["stop_conditions"]["max_tokens"]
        if max_tokens:
            sampling_params.max_tokens = max_tokens

        num_output_tokens_so_far = 0
        # TODO: Disable streaming for context only requests when adding disagg support
        async for res in self.engine.llm.generate_async(
            inputs=inputs, sampling_params=sampling_params, streaming=True
        ):
            # TRTLLM engine needs to start generating tokens first before stats
            # can be retrieved.
            if self.first_generation and self.publishers:
                self.publishers.start()
                self.first_generation = False

            if res.finished:
                yield {"finish_reason": "stop", "token_ids": []}
                break

            if not res.outputs:
                yield {"finish_reason": "error", "token_ids": []}
                break

            output = res.outputs[0]
            next_total_toks = len(output.token_ids)
            out = {"token_ids": output.token_ids[num_output_tokens_so_far:]}
            if output.finish_reason:
                out["finish_reason"] = output.finish_reason
            if output.stop_reason:
                out["stop_reason"] = output.stop_reason
            yield out
            num_output_tokens_so_far = next_total_toks


@dynamo_worker(static=False)
async def worker(runtime: DistributedRuntime):
    await init(runtime, cmd_line_args())


async def init(runtime: DistributedRuntime, config: Config):
    """
    Instantiate and serve
    """
    component = runtime.namespace(config.namespace).component(config.component)
    await component.create_service()

    # Convert model path to Path object if it's a local path, otherwise keep as string
    model_path = str(config.model_path)

    arg_map = {
        "model": model_path,
        "tensor_parallel_size": config.tensor_parallel_size,
        "skip_tokenizer_init": True,
        "disable_log_requests": True,
        "enable_prefix_caching": True,
        # KV routing relies on logging KV metrics
        "disable_log_stats": False,
    }
    if config.extra_engine_args != "":
        # TODO: Support extra engine args from json file as well.
        arg_map = update_llm_args_with_extra_options(arg_map, config.extra_engine_args)
    if config.publish_events_and_metrics:
        # 'event_buffer_max_size' is required to enable TRTLLM to publish kv cache events.
        kv_cache_config = None
        if "kv_cache_config" not in arg_map:
            kv_cache_config = {}
            kv_cache_config["event_buffer_max_size"] = DEFAULT_KV_EVENT_BUFFER_MAX_SIZE
        else:
            kv_cache_config = arg_map["kv_cache_config"]
            if not kv_cache_config.event_buffer_max_size:
                kv_cache_config.event_buffer_max_size = DEFAULT_KV_EVENT_BUFFER_MAX_SIZE
        arg_map["kv_cache_config"] = kv_cache_config

        # Only pytorch backend is supported for now to publish events and metrics.
        if "backend" not in arg_map:
            arg_map["backend"] = "pytorch"
        elif arg_map["backend"] != "pytorch":
            logging.error(
                "Only pytorch backend is supported for now to publish events and metrics."
            )
            sys.exit(1)

    logging.info(f"TRTLLM engine args: {arg_map}")
    engine_args = arg_map

    # Populate default sampling params from the model
    tokenizer = tokenizer_factory(arg_map["model"])
    default_sampling_params = SamplingParams()
    default_sampling_params._setup(tokenizer)
    default_sampling_params.stop = None

    async with get_tensorrtllm_engine(engine_args) as engine:
        endpoint = component.endpoint(config.endpoint)
        await register_llm(
            ModelType.Backend, endpoint, config.model_path, config.model_name
        )

        if config.publish_events_and_metrics:
            # Initialize and pass in the publishers to the request handler to
            # publish events and metrics.
            kv_listener = runtime.namespace(config.namespace).component(
                config.component
            )
            async with get_tensorrtllm_publisher(
                component,
                engine,
                kv_listener,
                int(endpoint.lease_id()),
                config.kv_block_size,
            ) as publisher:
                handler = RequestHandler(
                    component, engine, default_sampling_params, publisher
                )
                await endpoint.serve_endpoint(handler.generate)
        else:
            # No publishers, so just pass in None to the request handler.
            handler = RequestHandler(component, engine, default_sampling_params, None)
            await endpoint.serve_endpoint(handler.generate)


def cmd_line_args():
    parser = argparse.ArgumentParser(
        description="TensorRT-LLM server integrated with Dynamo LLM."
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
        "--tensor-parallel-size", type=int, default=1, help="Number of GPUs to use."
    )
    # IMPORTANT: We should ideally not expose this to users. We should be able to
    # query the block size from the TRTLLM engine.
    parser.add_argument(
        "--kv-block-size", type=int, default=32, help="Size of a KV cache block."
    )
    parser.add_argument(
        "--context-length",
        type=int,
        default=None,
        help="This argument is not used by TRTLLM. Please provide max_input_len, max_seq_len and max_output_len in yaml file and point --extra-engine-args to the yaml file.",
    )
    parser.add_argument(
        "--extra-engine-args",
        type=str,
        default="",
        help="Path to a YAML file containing additional keyword arguments to pass to the TRTLLM engine.",
    )
    parser.add_argument(
        "--publish-events-and-metrics",
        action="store_true",
        help="Publish events and metrics to the dynamo components.",
    )
    args = parser.parse_args()

    if args.context_length is not None:
        warnings.warn(
            "--context-length is accepted for compatibility but will be ignored for TensorRT-LLM. Please provide max_input_len, max_seq_len and max_output_len in yaml file and point --extra-engine-args to the yaml file.",
            UserWarning,
        )

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
    config.tensor_parallel_size = args.tensor_parallel_size
    config.kv_block_size = args.kv_block_size
    config.extra_engine_args = args.extra_engine_args
    config.publish_events_and_metrics = args.publish_events_and_metrics

    return config


if __name__ == "__main__":
    uvloop.install()
    try:
        asyncio.run(worker())
    except KeyboardInterrupt:
        logging.info("Received SIGINT, shutting down...")
