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


if __name__ == "__main__":
    unittest.main()
