import copy
import json
import logging
from typing import Any, Dict, Mapping, MutableMapping, Optional


logger = logging.getLogger(__name__)
logger.setLevel(logging.INFO)

LPR_ROUTER_RESOURCE_TYPE = "Lpr::Router::Service"

EXPORT_CONFIG_BUCKET_NAME = "LprConfigBucketName"
EXPORT_CONFIG_PUBLISHER_SERVICE_TOKEN = "LprConfigPublisherServiceToken"


def _as_env_kv_list(env: Mapping[str, Any]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for k, v in env.items():
        if not isinstance(k, str) or not k:
            raise ValueError("Environment keys must be non-empty strings.")
        if isinstance(v, (dict, str)):
            out.append({"Name": k, "Value": v})
            continue
        raise ValueError(f"Environment[{k}] must be a string (or an intrinsic function object).")
    return out


def _default_prefix_for(logical_id: str) -> Any:
    # Note: macro transforms can't take parameters via the Transform section, so we rely
    # on fixed exports and deterministic defaults.
    return {"Fn::Sub": f"lpr/${{AWS::StackName}}/{logical_id}/"}


def _import_value(name: str) -> dict[str, Any]:
    return {"Fn::ImportValue": name}


def _sub(template: str, variables: Optional[dict[str, Any]] = None) -> Any:
    if variables is None:
        return {"Fn::Sub": template}
    return {"Fn::Sub": [template, variables]}


def _get_att(logical_id: str, attr: str) -> dict[str, Any]:
    return {"Fn::GetAtt": [logical_id, attr]}

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


def _expand_router_service(
    *,
    resources: MutableMapping[str, Any],
    logical_id: str,
    original: Mapping[str, Any],
) -> None:
    props = original.get("Properties") or {}
    if not isinstance(props, dict):
        raise ValueError(f"{logical_id}.Properties must be an object.")

    if "ImageIdentifier" not in props:
        raise ValueError(f"{logical_id}.Properties.ImageIdentifier is required.")
    image_identifier = props["ImageIdentifier"]

    router_config = props.get("RouterConfig")
    if not isinstance(router_config, dict):
        raise ValueError(f"{logical_id}.Properties.RouterConfig is required and must be an object.")

    spec = props.get("Spec")
    if not isinstance(spec, dict):
        raise ValueError(f"{logical_id}.Properties.Spec is required and must be an object.")

    port = props.get("Port", "8080")
    auto_deploy = props.get("AutoDeploymentsEnabled", True)
    config_prefix = props.get("ConfigPrefix", _default_prefix_for(logical_id))

    env = props.get("Environment") or {}
    if not isinstance(env, dict):
        raise ValueError(f"{logical_id}.Properties.Environment must be an object.")

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

    observability_cfg = props.get("ObservabilityConfiguration")
    if observability_cfg is not None and not isinstance(observability_cfg, dict):
        raise ValueError(f"{logical_id}.Properties.ObservabilityConfiguration must be an object.")

    observability_arn = props.get("ObservabilityConfigurationArn")
    if observability_arn is not None and not isinstance(observability_arn, (str, dict)):
        raise ValueError(
            f"{logical_id}.Properties.ObservabilityConfigurationArn must be a string or intrinsic function object."
        )

    if observability_cfg is not None and observability_arn is not None:
        raise ValueError(
            f"{logical_id}.Properties.ObservabilityConfiguration and ObservabilityConfigurationArn are mutually exclusive."
        )

    observability_enabled = observability_cfg is not None or observability_arn is not None

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

    lpr_ecr_role_id = f"{logical_id}LprEcrAccessRole"
    lpr_instance_role_id = f"{logical_id}LprInstanceRole"
    lpr_publisher_id = f"{logical_id}LprConfigPublisher"
    lpr_autoscaling_id = f"{logical_id}LprAutoScaling"
    lpr_observability_id = f"{logical_id}LprObservability"

    for rid in (lpr_ecr_role_id, lpr_instance_role_id, lpr_publisher_id):
        _ensure_no_collision(resources, rid)

    if auto_scaling_cfg is not None:
        _ensure_no_collision(resources, lpr_autoscaling_id)
        resources[lpr_autoscaling_id] = {
            "Type": "AWS::AppRunner::AutoScalingConfiguration",
            "Properties": auto_scaling_cfg,
        }

    if observability_cfg is not None:
        _ensure_no_collision(resources, lpr_observability_id)
        resources[lpr_observability_id] = {
            "Type": "AWS::AppRunner::ObservabilityConfiguration",
            "Properties": observability_cfg,
        }

    resources[lpr_publisher_id] = {
        "Type": "Custom::LprConfigPublisher",
        "Properties": {
            "ServiceToken": _import_value(EXPORT_CONFIG_PUBLISHER_SERVICE_TOKEN),
            "Prefix": config_prefix,
            "Port": port,
            "RouterConfig": router_config,
            "Spec": spec,
        },
    }

    resources[lpr_ecr_role_id] = {
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
                "Sid": "ReadRouterConfig",
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

        managed_policy_arns = []
        if observability_enabled:
            managed_policy_arns.append("arn:aws:iam::aws:policy/AWSXRayDaemonWriteAccess")

        resources[lpr_instance_role_id] = {
            "Type": "AWS::IAM::Role",
            "Properties": {
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
                        "PolicyName": "LprRouterInstancePolicy",
                        "PolicyDocument": {
                            "Version": "2012-10-17",
                            "Statement": statements,
                        },
                    }
                ],
                **(
                    {"ManagedPolicyArns": managed_policy_arns}
                    if managed_policy_arns
                    else {}
                ),
            },
        }

        instance_role_arn = _get_att(lpr_instance_role_id, "Arn")

    runtime_env = dict(env)
    runtime_env.setdefault("AWS_REGION", {"Ref": "AWS::Region"})
    runtime_env.setdefault("AWS_DEFAULT_REGION", {"Ref": "AWS::Region"})
    runtime_env.setdefault("RUST_LOG", "info")
    runtime_env["LPR_CONFIG_URI"] = _get_att(lpr_publisher_id, "ConfigS3Uri")

    image_cfg: Dict[str, Any] = {
        "Port": port,
        "RuntimeEnvironmentVariables": _as_env_kv_list(runtime_env),
    }

    instance_cfg_final: Dict[str, Any] = {"InstanceRoleArn": instance_role_arn}
    if instance_cfg:
        for k, v in instance_cfg.items():
            if k == "InstanceRoleArn":
                continue
            instance_cfg_final[k] = v

    service_props: Dict[str, Any] = {
        "SourceConfiguration": {
            "AuthenticationConfiguration": {"AccessRoleArn": _get_att(lpr_ecr_role_id, "Arn")},
            "AutoDeploymentsEnabled": auto_deploy,
            "ImageRepository": {
                "ImageIdentifier": image_identifier,
                "ImageRepositoryType": "ECR",
                "ImageConfiguration": image_cfg,
            },
        },
        "InstanceConfiguration": instance_cfg_final,
    }

    if service_name is not None:
        service_props["ServiceName"] = service_name

    if auto_scaling_cfg is not None:
        service_props["AutoScalingConfigurationArn"] = _get_att(
            lpr_autoscaling_id, "AutoScalingConfigurationArn"
        )
    elif auto_scaling_arn is not None:
        service_props["AutoScalingConfigurationArn"] = auto_scaling_arn

    if observability_enabled:
        service_props["ObservabilityConfiguration"] = {
            "ObservabilityEnabled": True,
            "ObservabilityConfigurationArn": (
                _get_att(lpr_observability_id, "ObservabilityConfigurationArn")
                if observability_cfg is not None
                else observability_arn
            ),
        }

    # Replace the original resource in-place (same logical id) so `!GetAtt Router.ServiceUrl` keeps working.
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

        new_fragment = copy.deepcopy(fragment)
        new_resources = copy.deepcopy(resources)

        for logical_id, res in resources.items():
            if not isinstance(res, dict):
                continue
            if res.get("Type") != LPR_ROUTER_RESOURCE_TYPE:
                continue
            _expand_router_service(
                resources=new_resources,
                logical_id=logical_id,
                original=res,
            )

        new_fragment["Resources"] = new_resources

        logger.info(
            "Expanded %d %s resource(s)",
            sum(1 for r in resources.values() if isinstance(r, dict) and r.get("Type") == LPR_ROUTER_RESOURCE_TYPE),
            LPR_ROUTER_RESOURCE_TYPE,
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
