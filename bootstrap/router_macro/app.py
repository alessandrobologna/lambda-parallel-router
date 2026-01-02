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


def _ensure_no_collision(resources: Mapping[str, Any], logical_id: str) -> None:
    if logical_id in resources:
        raise ValueError(
            f"Macro expansion would overwrite an existing resource '{logical_id}'."
        )


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

    invoke_lambda_arns = props.get("InvokeLambdaArns") or []
    if not isinstance(invoke_lambda_arns, list):
        raise ValueError(f"{logical_id}.Properties.InvokeLambdaArns must be a list.")

    instance_role_arn = props.get("InstanceRoleArn")
    if instance_role_arn is not None and invoke_lambda_arns:
        raise ValueError(
            f"{logical_id}: cannot set both InstanceRoleArn and InvokeLambdaArns (macro can't attach policies to an imported role)."
        )

    service_name = props.get("ServiceName")

    lpr_ecr_role_id = f"{logical_id}LprEcrAccessRole"
    lpr_instance_role_id = f"{logical_id}LprInstanceRole"
    lpr_publisher_id = f"{logical_id}LprConfigPublisher"

    for rid in (lpr_ecr_role_id, lpr_instance_role_id, lpr_publisher_id):
        _ensure_no_collision(resources, rid)

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
            },
        }

        instance_role_arn = _get_att(lpr_instance_role_id, "Arn")

    runtime_env = dict(env)
    runtime_env.setdefault("AWS_REGION", {"Ref": "AWS::Region"})
    runtime_env.setdefault("AWS_DEFAULT_REGION", {"Ref": "AWS::Region"})
    runtime_env.setdefault("RUST_LOG", "info")
    runtime_env["LPR_CONFIG_S3_URI"] = _get_att(lpr_publisher_id, "ConfigS3Uri")

    image_cfg: Dict[str, Any] = {
        "Port": port,
        "RuntimeEnvironmentVariables": _as_env_kv_list(runtime_env),
    }

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
        "InstanceConfiguration": {"InstanceRoleArn": instance_role_arn},
    }

    if service_name is not None:
        service_props["ServiceName"] = service_name

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

