import copy
import json
import logging
import os
from typing import Any, Dict, Mapping, MutableMapping, Optional


logger = logging.getLogger(__name__)
logger.setLevel(logging.INFO)

SMUG_GATEWAY_RESOURCE_TYPE = "Smug::Gateway::Service"

EXPORT_CONFIG_BUCKET_NAME = "SmugConfigBucketName"
EXPORT_CONFIG_PUBLISHER_SERVICE_TOKEN = "SmugConfigPublisherServiceToken"

SMUG_OTEL_HEADERS_ENV_VAR = "SMUG_OTEL_HEADERS_JSON"
SMUG_OBSERVABILITY_VENDOR_ENV_VAR = "SMUG_OBSERVABILITY_VENDOR"
SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER_ENV_VAR = "SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER"


def _as_env_kv_list(env: Mapping[str, Any]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for k, v in env.items():
        if not isinstance(k, str) or not k:
            raise ValueError("Environment keys must be non-empty strings.")
        if k == "PORT":
            raise ValueError("PORT is a reserved environment variable name in App Runner.")
        if isinstance(v, (dict, str)):
            out.append({"Name": k, "Value": v})
            continue
        raise ValueError(f"Environment[{k}] must be a string (or an intrinsic function object).")
    return out


def _as_env_secret_kv_list(env: Mapping[str, Any]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for k, v in env.items():
        if not isinstance(k, str) or not k:
            raise ValueError("Environment secret keys must be non-empty strings.")
        if k == "PORT":
            raise ValueError("PORT is a reserved environment variable name in App Runner.")
        if isinstance(v, (dict, str)):
            out.append({"Name": k, "Value": v})
            continue
        raise ValueError(
            f"EnvironmentSecrets[{k}] must be a string (or an intrinsic function object)."
        )
    return out


def _default_prefix_for(logical_id: str) -> Any:
    # Note: macro transforms can't take parameters via the Transform section, so we rely
    # on fixed exports and deterministic defaults.
    return {"Fn::Sub": f"smug/${{AWS::StackName}}/{logical_id}/"}


def _import_value(name: str) -> dict[str, Any]:
    return {"Fn::ImportValue": name}


def _sub(template: str, variables: Optional[dict[str, Any]] = None) -> Any:
    if variables is None:
        return {"Fn::Sub": template}
    return {"Fn::Sub": [template, variables]}


def _get_att(logical_id: str, attr: str) -> dict[str, Any]:
    return {"Fn::GetAtt": [logical_id, attr]}


def _read_default_gateway_image_identifier() -> str:
    image = os.environ.get(SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER_ENV_VAR, "").strip()
    if not image:
        raise ValueError(
            f"Macro is missing environment variable {SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER_ENV_VAR}."
        )
    return image


def _intrinsic_equal(a: Any, b: Any) -> bool:
    if a is b:
        return True
    if type(a) is not type(b):
        return False
    if isinstance(a, dict):
        return json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)
    return a == b


def _ensure_no_collision(resources: Mapping[str, Any], logical_id: str) -> None:
    if logical_id in resources:
        raise ValueError(
            f"Macro expansion would overwrite an existing resource '{logical_id}'."
        )

def _collect_target_lambda_arns(spec: Mapping[str, Any]) -> list[Any]:
    paths = spec.get("paths") or {}
    if not isinstance(paths, dict):
        raise ValueError("Spec.paths must be an object.")

    found: list[Any] = []
    for path_item in paths.values():
        if not isinstance(path_item, dict):
            continue
        for op in path_item.values():
            if not isinstance(op, dict):
                continue
            if "x-target-lambda" not in op:
                continue
            target = op["x-target-lambda"]
            if not isinstance(target, (str, dict)):
                raise ValueError("x-target-lambda must be a string or an intrinsic function object.")
            if isinstance(target, str) and not target.startswith("arn:"):
                raise ValueError(
                    f"x-target-lambda must be a Lambda function ARN (got: {target!r})."
                )
            found.append(target)

    # De-duplicate while preserving order.
    out: list[Any] = []
    seen: set[str] = set()
    for v in found:
        key = v if isinstance(v, str) else json.dumps(v, sort_keys=True)
        if key in seen:
            continue
        seen.add(key)
        out.append(v)
    return out


def _split_secret_arns(values: list[Any]) -> tuple[list[Any], list[Any]]:
    secrets: list[Any] = []
    params: list[Any] = []

    for v in values:
        if isinstance(v, str):
            if not v.startswith("arn:"):
                raise ValueError(
                    "EnvironmentSecrets values must be ARNs (or intrinsic function objects resolving to ARNs)."
                )
            if ":secretsmanager:" in v:
                secrets.append(v)
            elif ":ssm:" in v:
                params.append(v)
            else:
                raise ValueError(
                    f"EnvironmentSecrets value must be a Secrets Manager or SSM Parameter Store ARN (got: {v!r})."
                )
            continue

        if isinstance(v, dict):
            # Intrinsics can resolve to either an SSM parameter ARN or a Secrets Manager ARN.
            # Add to both statements to keep the policy simple while still remaining scoped.
            secrets.append(v)
            params.append(v)
            continue

        raise ValueError(
            "EnvironmentSecrets values must be ARNs (or intrinsic function objects resolving to ARNs)."
        )

    return secrets, params


def _expand_gateway_service(
    *,
    resources: MutableMapping[str, Any],
    logical_id: str,
    original: Mapping[str, Any],
    template_parameter_values: Mapping[str, Any],
    template_parameters: Mapping[str, Any],
) -> None:
    props = original.get("Properties") or {}
    if not isinstance(props, dict):
        raise ValueError(f"{logical_id}.Properties must be an object.")

    image_identifier: Any = props.get("ImageIdentifier")
    if image_identifier is None or (isinstance(image_identifier, str) and not image_identifier.strip()):
        image_identifier = _read_default_gateway_image_identifier()
    elif isinstance(image_identifier, dict) and list(image_identifier.keys()) == ["Ref"]:
        param_name = image_identifier["Ref"]
        if isinstance(param_name, str) and param_name:
            raw_value = template_parameter_values.get(param_name)
            if isinstance(raw_value, str) and raw_value == "":
                image_identifier = _read_default_gateway_image_identifier()
            elif raw_value is None:
                # `templateParameterValues` may omit parameters that used their template defaults.
                param_def = template_parameters.get(param_name)
                if isinstance(param_def, dict) and param_def.get("Default") == "":
                    image_identifier = _read_default_gateway_image_identifier()

    if not isinstance(image_identifier, (str, dict)):
        raise ValueError(
            f"{logical_id}.Properties.ImageIdentifier must be a string or intrinsic function object."
        )

    gateway_config = props.get("GatewayConfig")
    if not isinstance(gateway_config, dict):
        raise ValueError(f"{logical_id}.Properties.GatewayConfig is required and must be an object.")

    spec = props.get("Spec")
    if not isinstance(spec, dict):
        raise ValueError(f"{logical_id}.Properties.Spec is required and must be an object.")

    port = props.get("Port", "8080")
    auto_deploy = props.get("AutoDeploymentsEnabled", True)
    config_prefix = props.get("ConfigPrefix", _default_prefix_for(logical_id))

    env = props.get("Environment") or {}
    if not isinstance(env, dict):
        raise ValueError(f"{logical_id}.Properties.Environment must be an object.")

    env_secrets = props.get("EnvironmentSecrets") or {}
    if not isinstance(env_secrets, dict):
        raise ValueError(f"{logical_id}.Properties.EnvironmentSecrets must be an object.")

    env_overlap = set(env.keys()) & set(env_secrets.keys())
    if env_overlap:
        overlap = ", ".join(sorted(env_overlap))
        raise ValueError(
            f"{logical_id}.Properties.Environment and EnvironmentSecrets contain duplicate keys: {overlap}"
        )

    instance_cfg = props.get("InstanceConfiguration")
    if instance_cfg is not None and not isinstance(instance_cfg, dict):
        raise ValueError(f"{logical_id}.Properties.InstanceConfiguration must be an object.")

    auto_scaling_cfg = props.get("AutoScalingConfiguration")
    if auto_scaling_cfg is not None and not isinstance(auto_scaling_cfg, dict):
        raise ValueError(f"{logical_id}.Properties.AutoScalingConfiguration must be an object.")

    auto_scaling_arn = props.get("AutoScalingConfigurationArn")
    if auto_scaling_arn is not None and not isinstance(auto_scaling_arn, (str, dict)):
        raise ValueError(
            f"{logical_id}.Properties.AutoScalingConfigurationArn must be a string or intrinsic function object."
        )

    if auto_scaling_cfg is not None and auto_scaling_arn is not None:
        raise ValueError(
            f"{logical_id}.Properties.AutoScalingConfiguration and AutoScalingConfigurationArn are mutually exclusive."
        )

    if "ObservabilityConfigurationArn" in props:
        raise ValueError(
            f"{logical_id}.Properties.ObservabilityConfigurationArn is not supported. Use ObservabilityConfiguration.Vendor instead."
        )

    observability_cfg = props.get("ObservabilityConfiguration")
    if observability_cfg is not None and not isinstance(observability_cfg, dict):
        raise ValueError(f"{logical_id}.Properties.ObservabilityConfiguration must be an object.")

    observability_vendor: Optional[str] = None
    otel_cfg: Optional[dict[str, Any]] = None
    if observability_cfg is not None:
        vendor = observability_cfg.get("Vendor")
        if vendor not in ("AWSXRAY", "OTEL"):
            raise ValueError(
                f"{logical_id}.Properties.ObservabilityConfiguration.Vendor must be AWSXRAY or OTEL."
            )
        observability_vendor = vendor

        raw_otel_cfg = observability_cfg.get("OpentelemetryConfiguration")
        if vendor == "AWSXRAY":
            if raw_otel_cfg is not None:
                raise ValueError(
                    f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration is not allowed when Vendor is AWSXRAY."
                )
        else:
            if raw_otel_cfg is None or not isinstance(raw_otel_cfg, dict):
                raise ValueError(
                    f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration is required when Vendor is OTEL."
                )
            otel_cfg = raw_otel_cfg

    apprunner_xray_enabled = observability_vendor == "AWSXRAY"

    otel_traces_endpoint: Any = None
    otel_protocol: Any = None
    otel_headers_secret_arn: Any = None
    if observability_vendor == "OTEL":
        assert otel_cfg is not None

        otel_traces_endpoint = otel_cfg.get("TracesEndpoint")
        if otel_traces_endpoint is None or not isinstance(otel_traces_endpoint, (str, dict)):
            raise ValueError(
                f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration.TracesEndpoint is required and must be a string (or an intrinsic function object)."
            )

        otel_protocol = otel_cfg.get("Protocol", "http/protobuf")
        if not isinstance(otel_protocol, (str, dict)):
            raise ValueError(
                f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration.Protocol must be a string (or an intrinsic function object)."
            )
        if isinstance(otel_protocol, str) and otel_protocol not in ("http/protobuf", "grpc"):
            raise ValueError(
                f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration.Protocol must be http/protobuf or grpc."
            )

        otel_headers_secret_arn = otel_cfg.get("HeadersSecretArn")
        if otel_headers_secret_arn is not None and not isinstance(otel_headers_secret_arn, (str, dict)):
            raise ValueError(
                f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration.HeadersSecretArn must be a string (or an intrinsic function object)."
            )
        if isinstance(otel_headers_secret_arn, str) and not otel_headers_secret_arn.startswith(
            "arn:aws:secretsmanager:"
        ):
            raise ValueError(
                f"{logical_id}.Properties.ObservabilityConfiguration.OpentelemetryConfiguration.HeadersSecretArn must be a Secrets Manager ARN."
            )

    effective_env_secrets = dict(env_secrets)
    if otel_headers_secret_arn is not None:
        if (
            SMUG_OTEL_HEADERS_ENV_VAR in effective_env_secrets
            and not _intrinsic_equal(effective_env_secrets[SMUG_OTEL_HEADERS_ENV_VAR], otel_headers_secret_arn)
        ):
            raise ValueError(
                f"{logical_id}.Properties.EnvironmentSecrets already defines {SMUG_OTEL_HEADERS_ENV_VAR}, which conflicts with OpentelemetryConfiguration.HeadersSecretArn."
            )
        effective_env_secrets.setdefault(SMUG_OTEL_HEADERS_ENV_VAR, otel_headers_secret_arn)

    instance_role_arn = props.get("InstanceRoleArn")
    if instance_cfg and "InstanceRoleArn" in instance_cfg:
        if instance_role_arn is None:
            instance_role_arn = instance_cfg["InstanceRoleArn"]
        elif not _intrinsic_equal(instance_role_arn, instance_cfg["InstanceRoleArn"]):
            raise ValueError(
                f"{logical_id}.Properties.InstanceRoleArn must match InstanceConfiguration.InstanceRoleArn when both are set."
            )
    invoke_lambda_arns = _collect_target_lambda_arns(spec)

    service_name = props.get("ServiceName")

    smug_ecr_role_id = f"{logical_id}SmugEcrAccessRole"
    smug_instance_role_id = f"{logical_id}SmugInstanceRole"
    smug_publisher_id = f"{logical_id}SmugConfigPublisher"
    smug_autoscaling_id = f"{logical_id}SmugAutoScaling"
    smug_observability_id = f"{logical_id}SmugObservability"

    for rid in (smug_ecr_role_id, smug_instance_role_id, smug_publisher_id):
        _ensure_no_collision(resources, rid)

    if auto_scaling_cfg is not None:
        _ensure_no_collision(resources, smug_autoscaling_id)
        resources[smug_autoscaling_id] = {
            "Type": "AWS::AppRunner::AutoScalingConfiguration",
            "Properties": auto_scaling_cfg,
        }

    if apprunner_xray_enabled:
        _ensure_no_collision(resources, smug_observability_id)
        resources[smug_observability_id] = {
            "Type": "AWS::AppRunner::ObservabilityConfiguration",
            "Properties": {
                "TraceConfiguration": {"Vendor": "AWSXRAY"},
            },
        }

    resources[smug_publisher_id] = {
        "Type": "Custom::SmugConfigPublisher",
        "Properties": {
            "ServiceToken": _import_value(EXPORT_CONFIG_PUBLISHER_SERVICE_TOKEN),
            "Prefix": config_prefix,
            "Port": port,
            "GatewayConfig": gateway_config,
            "Spec": spec,
        },
    }

    resources[smug_ecr_role_id] = {
        "Type": "AWS::IAM::Role",
        "Properties": {
            "AssumeRolePolicyDocument": {
                "Version": "2012-10-17",
                "Statement": [
                    {
                        "Effect": "Allow",
                        "Principal": {"Service": "build.apprunner.amazonaws.com"},
                        "Action": "sts:AssumeRole",
                    }
                ],
            },
            "ManagedPolicyArns": [
                "arn:aws:iam::aws:policy/service-role/AWSAppRunnerServicePolicyForECRAccess"
            ],
        },
    }

    if instance_role_arn is None:
        bucket_name = _import_value(EXPORT_CONFIG_BUCKET_NAME)
        object_arn = _sub(
            "arn:aws:s3:::${Bucket}/${Prefix}*",
            {"Bucket": bucket_name, "Prefix": config_prefix},
        )

        statements = [
            {
                "Sid": "ReadGatewayConfig",
                "Effect": "Allow",
                "Action": ["s3:GetObject"],
                "Resource": [object_arn],
            }
        ]

        if invoke_lambda_arns:
            statements.append(
                {
                    "Sid": "InvokeTargetLambdas",
                    "Effect": "Allow",
                    "Action": ["lambda:InvokeFunction", "lambda:InvokeWithResponseStream"],
                    "Resource": invoke_lambda_arns,
                }
            )

        if effective_env_secrets:
            secret_values = list(effective_env_secrets.values())
            secrets_arns, params_arns = _split_secret_arns(secret_values)

            if secrets_arns:
                statements.append(
                    {
                        "Sid": "ReadSecretsManagerSecrets",
                        "Effect": "Allow",
                        "Action": ["secretsmanager:GetSecretValue"],
                        "Resource": secrets_arns,
                    }
                )

            if params_arns:
                statements.append(
                    {
                        "Sid": "ReadSsmParameters",
                        "Effect": "Allow",
                        "Action": ["ssm:GetParameters"],
                        "Resource": params_arns,
                    }
                )

        managed_policy_arns = []
        if apprunner_xray_enabled:
            managed_policy_arns.append("arn:aws:iam::aws:policy/AWSXRayDaemonWriteAccess")

        instance_role_props = {
            "AssumeRolePolicyDocument": {
                "Version": "2012-10-17",
                "Statement": [
                    {
                        "Effect": "Allow",
                        "Principal": {"Service": "tasks.apprunner.amazonaws.com"},
                        "Action": "sts:AssumeRole",
                    }
                ],
            },
            "Policies": [
                {
                    "PolicyName": "SmugGatewayInstancePolicy",
                    "PolicyDocument": {
                        "Version": "2012-10-17",
                        "Statement": statements,
                    },
                }
            ],
        }
        if managed_policy_arns:
            instance_role_props["ManagedPolicyArns"] = managed_policy_arns

        resources[smug_instance_role_id] = {
            "Type": "AWS::IAM::Role",
            "Properties": instance_role_props,
        }

        instance_role_arn = _get_att(smug_instance_role_id, "Arn")

    runtime_env = dict(env)
    runtime_env.setdefault("AWS_REGION", {"Ref": "AWS::Region"})
    runtime_env.setdefault("AWS_DEFAULT_REGION", {"Ref": "AWS::Region"})
    runtime_env.setdefault("RUST_LOG", "info")
    runtime_env["SMUG_CONFIG_URI"] = _get_att(smug_publisher_id, "ConfigS3Uri")

    runtime_env_secrets = dict(effective_env_secrets)

    if observability_vendor is not None:
        runtime_env.setdefault(SMUG_OBSERVABILITY_VENDOR_ENV_VAR, observability_vendor)

    if apprunner_xray_enabled:
        runtime_env.setdefault("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4317")
        runtime_env.setdefault("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc")
        runtime_env.setdefault("OTEL_EXPORTER_OTLP_INSECURE", "true")
        runtime_env.setdefault("OTEL_PROPAGATORS", "xray,tracecontext,baggage")
        runtime_env.setdefault("OTEL_METRICS_EXPORTER", "none")
        if service_name is not None:
            runtime_env.setdefault("OTEL_SERVICE_NAME", service_name)

    if observability_vendor == "OTEL":
        runtime_env.setdefault("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", otel_traces_endpoint)
        runtime_env.setdefault("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", otel_protocol)
        runtime_env.setdefault("OTEL_PROPAGATORS", "xray,tracecontext,baggage")
        runtime_env.setdefault("OTEL_METRICS_EXPORTER", "none")
        if service_name is not None:
            runtime_env.setdefault("OTEL_SERVICE_NAME", service_name)

    image_cfg: Dict[str, Any] = {
        "Port": port,
        "RuntimeEnvironmentVariables": _as_env_kv_list(runtime_env),
    }
    if runtime_env_secrets:
        image_cfg["RuntimeEnvironmentSecrets"] = _as_env_secret_kv_list(runtime_env_secrets)

    instance_cfg_final: Dict[str, Any] = {"InstanceRoleArn": instance_role_arn}
    if instance_cfg:
        for k, v in instance_cfg.items():
            if k == "InstanceRoleArn":
                continue
            instance_cfg_final[k] = v

    service_props: Dict[str, Any] = {
        "SourceConfiguration": {
            "AuthenticationConfiguration": {"AccessRoleArn": _get_att(smug_ecr_role_id, "Arn")},
            "AutoDeploymentsEnabled": auto_deploy,
            "ImageRepository": {
                "ImageIdentifier": image_identifier,
                "ImageRepositoryType": "ECR",
                "ImageConfiguration": image_cfg,
            },
        },
        "InstanceConfiguration": instance_cfg_final,
        "HealthCheckConfiguration": {
            "Protocol": "HTTP",
            "Path": "/readyz",
            "Interval": 5,
            "Timeout": 2,
            "HealthyThreshold": 1,
            "UnhealthyThreshold": 1,
        },
    }

    if service_name is not None:
        service_props["ServiceName"] = service_name

    if auto_scaling_cfg is not None:
        service_props["AutoScalingConfigurationArn"] = _get_att(
            smug_autoscaling_id, "AutoScalingConfigurationArn"
        )
    elif auto_scaling_arn is not None:
        service_props["AutoScalingConfigurationArn"] = auto_scaling_arn

    if apprunner_xray_enabled:
        service_props["ObservabilityConfiguration"] = {
            "ObservabilityEnabled": True,
            "ObservabilityConfigurationArn": _get_att(
                smug_observability_id, "ObservabilityConfigurationArn"
            ),
        }

    # Replace the original resource in-place (same logical id) so `!GetAtt Gateway.ServiceUrl` keeps working.
    resources[logical_id] = {"Type": "AWS::AppRunner::Service", "Properties": service_props}


def handler(event: Mapping[str, Any], context: Any) -> Dict[str, Any]:
    request_id = event.get("requestId")
    fragment = event.get("fragment")

    if not isinstance(fragment, dict):
        return {
            "requestId": request_id,
            "status": "failed",
            "errorMessage": "Macro event fragment must be an object.",
            "fragment": fragment,
        }

    try:
        resources = fragment.get("Resources") or {}
        if not isinstance(resources, dict):
            raise ValueError("Template Resources must be an object.")
        template_parameters = fragment.get("Parameters") or {}
        if not isinstance(template_parameters, dict):
            raise ValueError("Template Parameters must be an object.")
        template_parameter_values = event.get("templateParameterValues") or {}
        if not isinstance(template_parameter_values, dict):
            raise ValueError("Macro event templateParameterValues must be an object.")

        new_fragment = copy.deepcopy(fragment)
        new_resources = copy.deepcopy(resources)

        for logical_id, res in resources.items():
            if not isinstance(res, dict):
                continue
            if res.get("Type") != SMUG_GATEWAY_RESOURCE_TYPE:
                continue
            _expand_gateway_service(
                resources=new_resources,
                logical_id=logical_id,
                original=res,
                template_parameter_values=template_parameter_values,
                template_parameters=template_parameters,
            )

        new_fragment["Resources"] = new_resources

        logger.info(
            "Expanded %d %s resource(s)",
            sum(1 for r in resources.values() if isinstance(r, dict) and r.get("Type") == SMUG_GATEWAY_RESOURCE_TYPE),
            SMUG_GATEWAY_RESOURCE_TYPE,
        )
        return {"requestId": request_id, "status": "success", "fragment": new_fragment}
    except Exception as exc:
        logger.error("Macro failed: %s", exc)
        logger.error("Event: %s", json.dumps(event))
        return {
            "requestId": request_id,
            "status": "failed",
            "errorMessage": str(exc),
            "fragment": fragment,
        }
