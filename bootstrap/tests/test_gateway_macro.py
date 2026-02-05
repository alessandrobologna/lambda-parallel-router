import unittest
import importlib.util
from pathlib import Path
import os

BOOTSTRAP_DIR = Path(__file__).resolve().parents[1]

_APP_PATH = BOOTSTRAP_DIR / "gateway_macro" / "app.py"
_SPEC = importlib.util.spec_from_file_location("gateway_macro_app", _APP_PATH)
assert _SPEC and _SPEC.loader
app = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(app)  # type: ignore[union-attr]


class GatewayMacroTests(unittest.TestCase):
    def test_defaults_image_identifier_from_macro_env(self) -> None:
        old = os.environ.get("SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER")
        os.environ["SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER"] = (
            "123456789012.dkr.ecr.us-east-1.amazonaws.com/simple-multiplexer-gateway/gateway:0.0.0"
        )
        event = {
            "requestId": "req-0",
            "fragment": {
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "GatewayConfig": {},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                }
            },
        }

        try:
            out = app.handler(event, context=None)
            self.assertEqual(out["status"], "success")
            resources = out["fragment"]["Resources"]
            image_id = (
                resources["Gateway"]["Properties"]["SourceConfiguration"]["ImageRepository"][
                    "ImageIdentifier"
                ]
            )
            self.assertEqual(
                image_id,
                "123456789012.dkr.ecr.us-east-1.amazonaws.com/simple-multiplexer-gateway/gateway:0.0.0",
            )
        finally:
            if old is None:
                os.environ.pop("SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER", None)
            else:
                os.environ["SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER"] = old

    def test_defaults_image_identifier_from_empty_ref_param(self) -> None:
        old = os.environ.get("SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER")
        os.environ["SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER"] = (
            "123456789012.dkr.ecr.us-east-1.amazonaws.com/simple-multiplexer-gateway/gateway:0.0.0"
        )
        event = {
            "requestId": "req-0b",
            "fragment": {
                "Parameters": {
                    "GatewayImageIdentifier": {
                        "Type": "String",
                        "Default": "",
                    }
                },
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "ImageIdentifier": {"Ref": "GatewayImageIdentifier"},
                            "GatewayConfig": {},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                },
            },
        }

        try:
            out = app.handler(event, context=None)
            self.assertEqual(out["status"], "success")
            resources = out["fragment"]["Resources"]
            image_id = (
                resources["Gateway"]["Properties"]["SourceConfiguration"]["ImageRepository"][
                    "ImageIdentifier"
                ]
            )
            self.assertEqual(
                image_id,
                "123456789012.dkr.ecr.us-east-1.amazonaws.com/simple-multiplexer-gateway/gateway:0.0.0",
            )
        finally:
            if old is None:
                os.environ.pop("SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER", None)
            else:
                os.environ["SMUG_DEFAULT_GATEWAY_IMAGE_IDENTIFIER"] = old

    def test_expands_gateway_resource(self) -> None:
        event = {
            "requestId": "req-1",
            "fragment": {
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "ImageIdentifier": {"Ref": "GatewayImageIdentifier"},
                            "Port": 8080,
                            "Environment": {"RUST_LOG": "debug"},
                            "GatewayConfig": {"MaxInflightInvocations": 1},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": {"Fn::GetAtt": ["Fn", "Arn"]},
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                }
            },
        }

        out = app.handler(event, context=None)
        self.assertEqual(out["status"], "success")
        frag = out["fragment"]
        resources = frag["Resources"]

        self.assertIn("Gateway", resources)
        self.assertEqual(resources["Gateway"]["Type"], "AWS::AppRunner::Service")

        self.assertIn("GatewaySmugConfigPublisher", resources)
        self.assertEqual(resources["GatewaySmugConfigPublisher"]["Type"], "Custom::SmugConfigPublisher")

        self.assertIn("GatewaySmugEcrAccessRole", resources)
        self.assertEqual(resources["GatewaySmugEcrAccessRole"]["Type"], "AWS::IAM::Role")

        self.assertIn("GatewaySmugInstanceRole", resources)
        self.assertEqual(resources["GatewaySmugInstanceRole"]["Type"], "AWS::IAM::Role")

        policy_doc = resources["GatewaySmugInstanceRole"]["Properties"]["Policies"][0]["PolicyDocument"]
        invoke_stmt = next(s for s in policy_doc["Statement"] if s.get("Sid") == "InvokeTargetLambdas")
        self.assertEqual(invoke_stmt["Action"], ["lambda:InvokeFunction", "lambda:InvokeWithResponseStream"])
        self.assertIn({"Fn::GetAtt": ["Fn", "Arn"]}, invoke_stmt["Resource"])

        svc_env = (
            resources["Gateway"]["Properties"]["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"][
                "RuntimeEnvironmentVariables"
            ]
        )
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["RUST_LOG"], "debug")
        self.assertIn("SMUG_CONFIG_URI", env_map)

        health = resources["Gateway"]["Properties"]["HealthCheckConfiguration"]
        self.assertEqual(health["Protocol"], "HTTP")
        self.assertEqual(health["Path"], "/readyz")
        self.assertEqual(health["HealthyThreshold"], 1)
        self.assertEqual(health["UnhealthyThreshold"], 1)

    def test_allows_instance_role_override(self) -> None:
        event = {
            "requestId": "req-2",
            "fragment": {
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "InstanceRoleArn": "arn:aws:iam::123:role/Existing",
                            "GatewayConfig": {},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                }
            },
        }

        out = app.handler(event, context=None)
        self.assertEqual(out["status"], "success")
        resources = out["fragment"]["Resources"]
        self.assertNotIn("GatewaySmugInstanceRole", resources)
        self.assertEqual(
            resources["Gateway"]["Properties"]["InstanceConfiguration"]["InstanceRoleArn"],
            "arn:aws:iam::123:role/Existing",
        )

    def test_supports_instance_and_autoscaling_config(self) -> None:
        event = {
            "requestId": "req-3",
            "fragment": {
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "GatewayConfig": {},
                            "InstanceConfiguration": {"Cpu": "0.5 vCPU", "Memory": "1 GB"},
                            "AutoScalingConfiguration": {
                                "AutoScalingConfigurationName": "smug-demo-autoscaling",
                                "MaxConcurrency": 200,
                                "MinSize": 2,
                                "MaxSize": 4,
                            },
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                }
            },
        }

        out = app.handler(event, context=None)
        self.assertEqual(out["status"], "success")
        resources = out["fragment"]["Resources"]

        self.assertIn("GatewaySmugAutoScaling", resources)
        self.assertEqual(
            resources["GatewaySmugAutoScaling"]["Type"], "AWS::AppRunner::AutoScalingConfiguration"
        )
        self.assertEqual(
            resources["GatewaySmugAutoScaling"]["Properties"]["MaxConcurrency"], 200
        )

        service_props = resources["Gateway"]["Properties"]
        self.assertEqual(
            service_props["AutoScalingConfigurationArn"],
            {"Fn::GetAtt": ["GatewaySmugAutoScaling", "AutoScalingConfigurationArn"]},
        )
        self.assertEqual(
            service_props["InstanceConfiguration"]["Cpu"],
            "0.5 vCPU",
        )
        self.assertEqual(
            service_props["InstanceConfiguration"]["Memory"],
            "1 GB",
        )

    def test_supports_observability_configuration_resource(self) -> None:
        event = {
            "requestId": "req-4",
            "fragment": {
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "GatewayConfig": {},
                            "ObservabilityConfiguration": {"Vendor": "AWSXRAY"},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                }
            },
        }

        out = app.handler(event, context=None)
        self.assertEqual(out["status"], "success")
        resources = out["fragment"]["Resources"]

        self.assertIn("GatewaySmugObservability", resources)
        self.assertEqual(
            resources["GatewaySmugObservability"]["Type"],
            "AWS::AppRunner::ObservabilityConfiguration",
        )

        service_props = resources["Gateway"]["Properties"]
        self.assertEqual(
            service_props["ObservabilityConfiguration"]["ObservabilityEnabled"],
            True,
        )
        self.assertEqual(
            service_props["ObservabilityConfiguration"]["ObservabilityConfigurationArn"],
            {"Fn::GetAtt": ["GatewaySmugObservability", "ObservabilityConfigurationArn"]},
        )

        svc_env = (
            resources["Gateway"]["Properties"]["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"][
                "RuntimeEnvironmentVariables"
            ]
        )
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["SMUG_OBSERVABILITY_VENDOR"], "AWSXRAY")
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://localhost:4317")
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL"], "grpc")
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_INSECURE"], "true")
        self.assertEqual(env_map["OTEL_PROPAGATORS"], "xray,tracecontext,baggage")
        self.assertEqual(env_map["OTEL_METRICS_EXPORTER"], "none")

        instance_role = resources["GatewaySmugInstanceRole"]["Properties"]
        self.assertIn("ManagedPolicyArns", instance_role)
        self.assertIn(
            "arn:aws:iam::aws:policy/AWSXRayDaemonWriteAccess",
            instance_role["ManagedPolicyArns"],
        )

    def test_supports_otlp_observability(self) -> None:
        event = {
            "requestId": "req-5",
            "fragment": {
                "Resources": {
                    "Gateway": {
                        "Type": "Smug::Gateway::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "GatewayConfig": {},
                            "ObservabilityConfiguration": {
                                "Vendor": "OTEL",
                                "OpentelemetryConfiguration": {
                                    "TracesEndpoint": "https://ingest.example.com/v1/traces",
                                    "Protocol": "http/protobuf",
                                    "HeadersSecretArn": "arn:aws:secretsmanager:us-east-1:123:secret:otel-headers",
                                },
                            },
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-smug": {"maxWaitMs": 1, "maxBatchSize": 1},
                                        }
                                    }
                                },
                            },
                        },
                    }
                }
            },
        }

        out = app.handler(event, context=None)
        self.assertEqual(out["status"], "success")
        resources = out["fragment"]["Resources"]

        self.assertNotIn("GatewaySmugObservability", resources)

        service_props = resources["Gateway"]["Properties"]
        self.assertNotIn("ObservabilityConfiguration", service_props)

        image_cfg = service_props["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"]
        svc_env = image_cfg["RuntimeEnvironmentVariables"]
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["SMUG_OBSERVABILITY_VENDOR"], "OTEL")
        self.assertEqual(
            env_map["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"], "https://ingest.example.com/v1/traces"
        )
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL"], "http/protobuf")
        self.assertEqual(env_map["OTEL_PROPAGATORS"], "xray,tracecontext,baggage")
        self.assertEqual(env_map["OTEL_METRICS_EXPORTER"], "none")

        runtime_secrets = image_cfg["RuntimeEnvironmentSecrets"]
        secret_map = {kv["Name"]: kv["Value"] for kv in runtime_secrets}
        self.assertEqual(
            secret_map["SMUG_OTEL_HEADERS_JSON"],
            "arn:aws:secretsmanager:us-east-1:123:secret:otel-headers",
        )

        policy_doc = resources["GatewaySmugInstanceRole"]["Properties"]["Policies"][0]["PolicyDocument"]
        secrets_stmt = next(
            s for s in policy_doc["Statement"] if s.get("Sid") == "ReadSecretsManagerSecrets"
        )
        self.assertIn(
            "arn:aws:secretsmanager:us-east-1:123:secret:otel-headers",
            secrets_stmt["Resource"],
        )


if __name__ == "__main__":
    unittest.main()
