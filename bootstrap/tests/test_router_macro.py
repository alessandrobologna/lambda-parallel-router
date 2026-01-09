import unittest
import importlib.util
from pathlib import Path

BOOTSTRAP_DIR = Path(__file__).resolve().parents[1]

_APP_PATH = BOOTSTRAP_DIR / "router_macro" / "app.py"
_SPEC = importlib.util.spec_from_file_location("router_macro_app", _APP_PATH)
assert _SPEC and _SPEC.loader
app = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(app)  # type: ignore[union-attr]


class RouterMacroTests(unittest.TestCase):
    def test_expands_router_resource(self) -> None:
        event = {
            "requestId": "req-1",
            "fragment": {
                "Resources": {
                    "Router": {
                        "Type": "Lpr::Router::Service",
                        "Properties": {
                            "ImageIdentifier": {"Ref": "RouterImageIdentifier"},
                            "Port": 8080,
                            "Environment": {"RUST_LOG": "debug"},
                            "RouterConfig": {"MaxInflightInvocations": 1},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": {"Fn::GetAtt": ["Fn", "Arn"]},
                                            "x-lpr": {"maxWaitMs": 1, "maxBatchSize": 1},
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

        self.assertIn("Router", resources)
        self.assertEqual(resources["Router"]["Type"], "AWS::AppRunner::Service")

        self.assertIn("RouterLprConfigPublisher", resources)
        self.assertEqual(resources["RouterLprConfigPublisher"]["Type"], "Custom::LprConfigPublisher")

        self.assertIn("RouterLprEcrAccessRole", resources)
        self.assertEqual(resources["RouterLprEcrAccessRole"]["Type"], "AWS::IAM::Role")

        self.assertIn("RouterLprInstanceRole", resources)
        self.assertEqual(resources["RouterLprInstanceRole"]["Type"], "AWS::IAM::Role")

        policy_doc = resources["RouterLprInstanceRole"]["Properties"]["Policies"][0]["PolicyDocument"]
        invoke_stmt = next(s for s in policy_doc["Statement"] if s.get("Sid") == "InvokeTargetLambdas")
        self.assertEqual(invoke_stmt["Action"], ["lambda:InvokeFunction", "lambda:InvokeWithResponseStream"])
        self.assertIn({"Fn::GetAtt": ["Fn", "Arn"]}, invoke_stmt["Resource"])

        svc_env = (
            resources["Router"]["Properties"]["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"][
                "RuntimeEnvironmentVariables"
            ]
        )
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["RUST_LOG"], "debug")
        self.assertIn("LPR_CONFIG_URI", env_map)

    def test_allows_instance_role_override(self) -> None:
        event = {
            "requestId": "req-2",
            "fragment": {
                "Resources": {
                    "Router": {
                        "Type": "Lpr::Router::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "InstanceRoleArn": "arn:aws:iam::123:role/Existing",
                            "RouterConfig": {},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-lpr": {"maxWaitMs": 1, "maxBatchSize": 1},
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
        self.assertNotIn("RouterLprInstanceRole", resources)
        self.assertEqual(
            resources["Router"]["Properties"]["InstanceConfiguration"]["InstanceRoleArn"],
            "arn:aws:iam::123:role/Existing",
        )

    def test_supports_instance_and_autoscaling_config(self) -> None:
        event = {
            "requestId": "req-3",
            "fragment": {
                "Resources": {
                    "Router": {
                        "Type": "Lpr::Router::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "RouterConfig": {},
                            "InstanceConfiguration": {"Cpu": "0.5 vCPU", "Memory": "1 GB"},
                            "AutoScalingConfiguration": {
                                "AutoScalingConfigurationName": "lpr-demo-autoscaling",
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
                                            "x-lpr": {"maxWaitMs": 1, "maxBatchSize": 1},
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

        self.assertIn("RouterLprAutoScaling", resources)
        self.assertEqual(
            resources["RouterLprAutoScaling"]["Type"], "AWS::AppRunner::AutoScalingConfiguration"
        )
        self.assertEqual(
            resources["RouterLprAutoScaling"]["Properties"]["MaxConcurrency"], 200
        )

        service_props = resources["Router"]["Properties"]
        self.assertEqual(
            service_props["AutoScalingConfigurationArn"],
            {"Fn::GetAtt": ["RouterLprAutoScaling", "AutoScalingConfigurationArn"]},
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
                    "Router": {
                        "Type": "Lpr::Router::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "RouterConfig": {},
                            "ObservabilityConfiguration": {"Vendor": "AWSXRAY"},
                            "Spec": {
                                "openapi": "3.0.0",
                                "paths": {
                                    "/hello": {
                                        "get": {
                                            "x-target-lambda": "arn:aws:lambda:us-east-1:123:function:fn",
                                            "x-lpr": {"maxWaitMs": 1, "maxBatchSize": 1},
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

        self.assertIn("RouterLprObservability", resources)
        self.assertEqual(
            resources["RouterLprObservability"]["Type"],
            "AWS::AppRunner::ObservabilityConfiguration",
        )

        service_props = resources["Router"]["Properties"]
        self.assertEqual(
            service_props["ObservabilityConfiguration"]["ObservabilityEnabled"],
            True,
        )
        self.assertEqual(
            service_props["ObservabilityConfiguration"]["ObservabilityConfigurationArn"],
            {"Fn::GetAtt": ["RouterLprObservability", "ObservabilityConfigurationArn"]},
        )

        svc_env = (
            resources["Router"]["Properties"]["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"][
                "RuntimeEnvironmentVariables"
            ]
        )
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["LPR_OBSERVABILITY_VENDOR"], "AWSXRAY")
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://localhost:4317")
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL"], "grpc")
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_INSECURE"], "true")
        self.assertEqual(env_map["OTEL_PROPAGATORS"], "xray,tracecontext,baggage")
        self.assertEqual(env_map["OTEL_METRICS_EXPORTER"], "none")

        instance_role = resources["RouterLprInstanceRole"]["Properties"]
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
                    "Router": {
                        "Type": "Lpr::Router::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "RouterConfig": {},
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
                                            "x-lpr": {"maxWaitMs": 1, "maxBatchSize": 1},
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

        self.assertNotIn("RouterLprObservability", resources)

        service_props = resources["Router"]["Properties"]
        self.assertNotIn("ObservabilityConfiguration", service_props)

        image_cfg = service_props["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"]
        svc_env = image_cfg["RuntimeEnvironmentVariables"]
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["LPR_OBSERVABILITY_VENDOR"], "OTEL")
        self.assertEqual(
            env_map["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"], "https://ingest.example.com/v1/traces"
        )
        self.assertEqual(env_map["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL"], "http/protobuf")
        self.assertEqual(env_map["OTEL_PROPAGATORS"], "xray,tracecontext,baggage")
        self.assertEqual(env_map["OTEL_METRICS_EXPORTER"], "none")

        runtime_secrets = image_cfg["RuntimeEnvironmentSecrets"]
        secret_map = {kv["Name"]: kv["Value"] for kv in runtime_secrets}
        self.assertEqual(
            secret_map["LPR_OTEL_HEADERS_JSON"],
            "arn:aws:secretsmanager:us-east-1:123:secret:otel-headers",
        )

        policy_doc = resources["RouterLprInstanceRole"]["Properties"]["Policies"][0]["PolicyDocument"]
        secrets_stmt = next(
            s for s in policy_doc["Statement"] if s.get("Sid") == "ReadSecretsManagerSecrets"
        )
        self.assertIn(
            "arn:aws:secretsmanager:us-east-1:123:secret:otel-headers",
            secrets_stmt["Resource"],
        )


if __name__ == "__main__":
    unittest.main()
