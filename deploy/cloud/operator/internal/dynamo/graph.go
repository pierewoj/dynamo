/*
 * SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package dynamo

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"

	"emperror.dev/errors"
	apiStoreClient "github.com/ai-dynamo/dynamo/deploy/cloud/operator/api/dynamo/api_store_client"
	"github.com/ai-dynamo/dynamo/deploy/cloud/operator/api/dynamo/common"
	"github.com/ai-dynamo/dynamo/deploy/cloud/operator/api/dynamo/schemas"
	"github.com/ai-dynamo/dynamo/deploy/cloud/operator/api/v1alpha1"
	commonconfig "github.com/ai-dynamo/dynamo/deploy/cloud/operator/internal/config"
	commonconsts "github.com/ai-dynamo/dynamo/deploy/cloud/operator/internal/consts"
	"github.com/huandu/xstrings"
	corev1 "k8s.io/api/core/v1"
	k8serrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/runtime"
	"sigs.k8s.io/controller-runtime/pkg/log"

	"github.com/ai-dynamo/dynamo/deploy/cloud/operator/internal/archive"
	"gopkg.in/yaml.v2"
)

const (
	ComponentTypePlanner      = "planner"
	PlannerServiceAccountName = "planner-serviceaccount"
)

// ServiceConfig represents the YAML configuration structure for a service
type DynamoConfig struct {
	Enabled       bool   `yaml:"enabled"`
	Namespace     string `yaml:"namespace"`
	Name          string `yaml:"name"`
	ComponentType string `yaml:"component_type,omitempty"`
}

type Resources struct {
	CPU    *string           `yaml:"cpu,omitempty" json:"cpu,omitempty"`
	Memory *string           `yaml:"memory,omitempty" json:"memory,omitempty"`
	GPU    *string           `yaml:"gpu,omitempty" json:"gpu,omitempty"`
	Custom map[string]string `yaml:"custom,omitempty" json:"custom,omitempty"`
}

type Traffic struct {
	Timeout int `yaml:"timeout"`
}

type Autoscaling struct {
	MinReplicas int `yaml:"min_replicas"`
	MaxReplicas int `yaml:"max_replicas"`
}

type Config struct {
	Dynamo       *DynamoConfig `yaml:"dynamo,omitempty"`
	Resources    *Resources    `yaml:"resources,omitempty"`
	Traffic      *Traffic      `yaml:"traffic,omitempty"`
	Autoscaling  *Autoscaling  `yaml:"autoscaling,omitempty"`
	HttpExposed  bool          `yaml:"http_exposed,omitempty"`
	ApiEndpoints []string      `yaml:"api_endpoints,omitempty"`
	Workers      *int32        `yaml:"workers,omitempty"`
	TotalGpus    *int32        `yaml:"total_gpus,omitempty"`
}

type ServiceConfig struct {
	Name         string              `yaml:"name"`
	Dependencies []map[string]string `yaml:"dependencies,omitempty"`
	Config       Config              `yaml:"config"`
}

type DynDeploymentConfig = map[string]*DynDeploymentServiceConfig

// ServiceConfig represents the configuration for a specific service
type DynDeploymentServiceConfig struct {
	ServiceArgs *ServiceArgs `json:"ServiceArgs,omitempty"`
}

// ServiceArgs represents the arguments that can be passed to any service
type ServiceArgs struct {
	Workers   *int32     `json:"workers,omitempty"`
	Resources *Resources `json:"resources,omitempty"`
	TotalGpus *int32     `json:"total_gpus,omitempty"`
}

func (s ServiceConfig) GetNamespace() *string {
	if s.Config.Dynamo == nil || s.Config.Dynamo.Namespace == "" {
		return nil
	}
	return &s.Config.Dynamo.Namespace
}

func GetDefaultDynamoNamespace(ctx context.Context, dynamoDeployment *v1alpha1.DynamoGraphDeployment) string {
	return fmt.Sprintf("dynamo-%s", dynamoDeployment.Name)
}

func RetrieveDynamoGraphDownloadURL(ctx context.Context, dynamoDeployment *v1alpha1.DynamoGraphDeployment, recorder EventRecorder) (*string, error) {
	dynamoGraphDownloadURL := ""
	var dynamoComponent *schemas.DynamoComponent
	dynamoComponentRepositoryName, _, dynamoComponentVersion := xstrings.Partition(dynamoDeployment.Spec.DynamoGraph, ":")

	var err error
	var apiStoreClient *apiStoreClient.ApiStoreClient
	var apiStoreConf *commonconfig.ApiStoreConfig

	apiStoreClient, apiStoreConf, err = GetApiStoreClient(ctx)
	if err != nil {
		err = errors.Wrap(err, "get api store client")
		return nil, err
	}

	if apiStoreClient == nil || apiStoreConf == nil {
		err = errors.New("can't get api store client, please check api store configuration")
		return nil, err
	}

	recorder.Eventf(dynamoDeployment, corev1.EventTypeNormal, "GenerateImageBuilderPod", "Getting dynamo graph %s from api store service", dynamoDeployment.Spec.DynamoGraph)
	dynamoComponent, err = apiStoreClient.GetDynamoComponent(ctx, dynamoComponentRepositoryName, dynamoComponentVersion)
	if err != nil {
		err = errors.Wrap(err, "get dynamo component")
		return nil, err
	}
	recorder.Eventf(dynamoDeployment, corev1.EventTypeNormal, "GenerateImageBuilderPod", "Got dynamo graph %s from api store service", dynamoDeployment.Spec.DynamoGraph)

	if dynamoComponent.TransmissionStrategy != nil && *dynamoComponent.TransmissionStrategy == schemas.TransmissionStrategyPresignedURL {
		var dynamoComponent_ *schemas.DynamoComponent
		recorder.Eventf(dynamoDeployment, corev1.EventTypeNormal, "GenerateImageBuilderPod", "Getting presigned url for dynamo graph %s from api store service", dynamoDeployment.Spec.DynamoGraph)
		dynamoComponent_, err = apiStoreClient.PresignDynamoComponentDownloadURL(ctx, dynamoComponentRepositoryName, dynamoComponentVersion)
		if err != nil {
			err = errors.Wrap(err, "presign dynamo component download url")
			return nil, err
		}
		recorder.Eventf(dynamoDeployment, corev1.EventTypeNormal, "GenerateImageBuilderPod", "Got presigned url for dynamo graph %s from api store service", dynamoDeployment.Spec.DynamoGraph)
		dynamoGraphDownloadURL = dynamoComponent_.PresignedDownloadUrl
	} else {
		dynamoGraphDownloadURL = fmt.Sprintf("%s/api/v1/dynamo_components/%s/versions/%s/download", apiStoreConf.Endpoint, dynamoComponentRepositoryName, dynamoComponentVersion)
	}

	return &dynamoGraphDownloadURL, nil
}

// ServicesConfig represents the top-level YAML structure of a dynamoComponent yaml file stored in a dynamoComponent tar file
type DynamoGraphConfig struct {
	DynamoTag    string          `yaml:"service"`
	Services     []ServiceConfig `yaml:"services"`
	EntryService string          `yaml:"entry_service"`
}

type EventRecorder interface {
	Eventf(obj runtime.Object, eventtype string, reason string, message string, args ...interface{})
}

func RetrieveDynamoGraphConfigurationFile(ctx context.Context, url string) (*bytes.Buffer, error) {
	req, err := http.NewRequest("GET", url, nil)
	if err != nil {
		return nil, err
	}

	client := &http.Client{}
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer func() {
		if err := resp.Body.Close(); err != nil {
			logger := log.FromContext(ctx)
			logger.Error(err, "error closing response body")
		}
	}()

	// Read the tar file into memory
	tarData, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}

	// Extract the YAML file
	yamlFileName := "dynamo.yaml"
	yamlContent, err := archive.ExtractFileFromTar(tarData, yamlFileName)
	if err != nil {
		return nil, err
	}

	return yamlContent, nil
}

func GetApiStoreClient(ctx context.Context) (*apiStoreClient.ApiStoreClient, *commonconfig.ApiStoreConfig, error) {
	apiStoreConf, err := commonconfig.GetApiStoreConfig(ctx)
	isNotFound := k8serrors.IsNotFound(err)
	if err != nil && !isNotFound {
		err = errors.Wrap(err, "get api store config")
		return nil, nil, err
	}

	if isNotFound {
		return nil, nil, errors.New("endpoint config not found")
	}

	if apiStoreConf.Endpoint == "" {
		return nil, nil, errors.New("endpoint is empty")
	}

	if apiStoreConf.ClusterName == "" {
		apiStoreConf.ClusterName = "default"
	}

	apiStoreClient := apiStoreClient.NewApiStoreClient(apiStoreConf.Endpoint)

	return apiStoreClient, apiStoreConf, nil
}

func ParseDynamoGraphConfig(ctx context.Context, yamlContent *bytes.Buffer) (*DynamoGraphConfig, error) {
	var config DynamoGraphConfig
	logger := log.FromContext(ctx)
	logger.Info("trying to parse dynamo graph config", "yamlContent", yamlContent.String())
	err := yaml.Unmarshal(yamlContent.Bytes(), &config)
	return &config, err
}

func ParseDynDeploymentConfig(ctx context.Context, jsonContent []byte) (DynDeploymentConfig, error) {
	var config DynDeploymentConfig
	err := json.Unmarshal(jsonContent, &config)
	return config, err
}

func GetDynamoGraphConfig(ctx context.Context, dynamoDeployment *v1alpha1.DynamoGraphDeployment, recorder EventRecorder) (*DynamoGraphConfig, error) {
	dynamoGraphDownloadURL, err := RetrieveDynamoGraphDownloadURL(ctx, dynamoDeployment, recorder)
	if err != nil {
		return nil, err
	}
	yamlContent, err := RetrieveDynamoGraphConfigurationFile(ctx, *dynamoGraphDownloadURL)
	if err != nil {
		return nil, err
	}
	return ParseDynamoGraphConfig(ctx, yamlContent)
}

func SetLwsAnnotations(serviceArgs *ServiceArgs, deployment *v1alpha1.DynamoComponentDeployment) error {
	if serviceArgs.Resources != nil &&
		serviceArgs.Resources.GPU != nil && *serviceArgs.Resources.GPU != "" && *serviceArgs.Resources.GPU != "0" &&
		serviceArgs.TotalGpus != nil && *serviceArgs.TotalGpus > 0 {

		gpusPerNodeStr := *serviceArgs.Resources.GPU
		gpusPerNode, errGpusPerNode := strconv.Atoi(gpusPerNodeStr)

		if errGpusPerNode != nil {
			return fmt.Errorf("failed to parse GPUs per node value '%s' for service %s: %w", gpusPerNodeStr, deployment.Spec.ServiceName, errGpusPerNode)
		}

		// Calculate lwsSize using ceiling division to ensure enough nodes for all GPUs
		lwsSize := (int(*serviceArgs.TotalGpus) + gpusPerNode - 1) / gpusPerNode
		if lwsSize > 1 {
			if deployment.Spec.Annotations == nil {
				deployment.Spec.Annotations = make(map[string]string)
			}
			deployment.Spec.Annotations["nvidia.com/lws-size"] = strconv.Itoa(lwsSize)
			deployment.Spec.Annotations["nvidia.com/deployment-type"] = "leader-worker"
		}
	}
	return nil
}

// GenerateDynamoComponentsDeployments generates a map of DynamoComponentDeployments from a DynamoGraphConfig
func GenerateDynamoComponentsDeployments(ctx context.Context, parentDynamoGraphDeployment *v1alpha1.DynamoGraphDeployment, config *DynamoGraphConfig, ingressSpec *v1alpha1.IngressSpec) (map[string]*v1alpha1.DynamoComponentDeployment, error) {
	dynamoServices := make(map[string]string)
	deployments := make(map[string]*v1alpha1.DynamoComponentDeployment)
	graphDynamoNamespace := ""
	for _, service := range config.Services {
		deployment := &v1alpha1.DynamoComponentDeployment{}
		deployment.Name = fmt.Sprintf("%s-%s", parentDynamoGraphDeployment.Name, strings.ToLower(service.Name))
		deployment.Namespace = parentDynamoGraphDeployment.Namespace
		deployment.Spec.DynamoTag = config.DynamoTag
		deployment.Spec.DynamoComponent = parentDynamoGraphDeployment.Spec.DynamoGraph
		deployment.Spec.ServiceName = service.Name
		deployment.Spec.Replicas = service.Config.Workers
		labels := make(map[string]string)
		// add the labels in the spec in order to label all sub-resources
		deployment.Spec.Labels = labels
		// and add the labels to the deployment itself
		deployment.Labels = labels
		labels[commonconsts.KubeLabelDynamoComponent] = service.Name
		if service.Config.Dynamo != nil && service.Config.Dynamo.Enabled {
			dynamoNamespace := service.Config.Dynamo.Namespace
			if dynamoNamespace == "" {
				// if no namespace is specified, use the default namespace
				dynamoNamespace = GetDefaultDynamoNamespace(ctx, parentDynamoGraphDeployment)
			}
			deployment.Spec.DynamoNamespace = &dynamoNamespace
			dynamoServices[service.Name] = fmt.Sprintf("%s/%s", service.Config.Dynamo.Name, dynamoNamespace)
			labels[commonconsts.KubeLabelDynamoNamespace] = dynamoNamespace
			// we check that all dynamo components are in the same namespace
			// this is needed for the planner to work correctly
			// this check will be removed when the global planner will be implemented
			if graphDynamoNamespace != "" && graphDynamoNamespace != dynamoNamespace {
				return nil, fmt.Errorf("different namespaces for the same graph, expected %s, got %s", graphDynamoNamespace, dynamoNamespace)
			}
			graphDynamoNamespace = dynamoNamespace
			if service.Config.Dynamo.ComponentType == ComponentTypePlanner {
				deployment.Spec.ExtraPodSpec = &common.ExtraPodSpec{
					ServiceAccountName: PlannerServiceAccountName,
				}
			}
		}
		// Check http_exposed independently
		if config.EntryService == service.Name && service.Config.HttpExposed {
			deployment.Spec.Ingress = *ingressSpec
			// TODO (maybe): add paths to IngressSpec
		}

		if service.Config.Resources != nil {
			deployment.Spec.Resources = &common.Resources{
				Requests: &common.ResourceItem{
					Custom: service.Config.Resources.Custom,
				},
				Limits: &common.ResourceItem{
					Custom: service.Config.Resources.Custom,
				},
			}
			if service.Config.Resources.CPU != nil {
				deployment.Spec.Resources.Requests.CPU = *service.Config.Resources.CPU
				deployment.Spec.Resources.Limits.CPU = *service.Config.Resources.CPU
			}
			if service.Config.Resources.Memory != nil {
				deployment.Spec.Resources.Requests.Memory = *service.Config.Resources.Memory
				deployment.Spec.Resources.Limits.Memory = *service.Config.Resources.Memory
			}
			if service.Config.Resources.GPU != nil {
				deployment.Spec.Resources.Requests.GPU = *service.Config.Resources.GPU
				deployment.Spec.Resources.Limits.GPU = *service.Config.Resources.GPU
			}

			serviceArgs := ServiceArgs{
				Resources: service.Config.Resources,
				TotalGpus: service.Config.TotalGpus,
				Workers:   service.Config.Workers,
			}
			if err := SetLwsAnnotations(&serviceArgs, deployment); err != nil {
				return nil, err
			}
		}
		deployment.Spec.Autoscaling = &v1alpha1.Autoscaling{
			Enabled: false,
		}
		if service.Config.Autoscaling != nil {
			deployment.Spec.Autoscaling.Enabled = true
			deployment.Spec.Autoscaling.MinReplicas = service.Config.Autoscaling.MinReplicas
			deployment.Spec.Autoscaling.MaxReplicas = service.Config.Autoscaling.MaxReplicas
		}
		deployments[service.Name] = deployment
	}
	for _, service := range config.Services {
		deployment := deployments[service.Name]
		// generate external services
		for _, dependency := range service.Dependencies {
			dependentServiceName := dependency["service"]
			if deployment.Spec.ExternalServices == nil {
				deployment.Spec.ExternalServices = make(map[string]v1alpha1.ExternalService)
			}
			dependencyDeployment := deployments[dependentServiceName]
			if dependencyDeployment == nil {
				return nil, fmt.Errorf("dependency %s not found", dependentServiceName)
			}
			if dynamoService, ok := dynamoServices[dependentServiceName]; ok {
				deployment.Spec.ExternalServices[dependentServiceName] = v1alpha1.ExternalService{
					DeploymentSelectorKey:   "dynamo",
					DeploymentSelectorValue: dynamoService,
				}
			} else {
				deployment.Spec.ExternalServices[dependentServiceName] = v1alpha1.ExternalService{
					DeploymentSelectorKey:   "name",
					DeploymentSelectorValue: dependentServiceName,
				}
			}
		}
	}
	return deployments, nil
}
