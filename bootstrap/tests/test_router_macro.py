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
                            "InvokeLambdaArns": [{"Fn::GetAtt": ["Fn", "Arn"]}],
                            "RouterConfig": {"max_inflight_invocations": 1},
                            "Spec": {"openapi": "3.0.0", "paths": {}},
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

        svc_env = (
            resources["Router"]["Properties"]["SourceConfiguration"]["ImageRepository"]["ImageConfiguration"][
                "RuntimeEnvironmentVariables"
            ]
        )
        env_map = {kv["Name"]: kv["Value"] for kv in svc_env}
        self.assertEqual(env_map["RUST_LOG"], "debug")
        self.assertIn("LPR_CONFIG_S3_URI", env_map)

    def test_rejects_instance_role_and_invoke_list(self) -> None:
        event = {
            "requestId": "req-2",
            "fragment": {
                "Resources": {
                    "Router": {
                        "Type": "Lpr::Router::Service",
                        "Properties": {
                            "ImageIdentifier": "x",
                            "InstanceRoleArn": "arn:aws:iam::123:role/Existing",
                            "InvokeLambdaArns": ["arn:aws:lambda:us-east-1:123:function:fn"],
                            "RouterConfig": {},
                            "Spec": {"openapi": "3.0.0", "paths": {}},
                        },
                    }
                }
            },
        }

        out = app.handler(event, context=None)
        self.assertEqual(out["status"], "failed")


if __name__ == "__main__":
    unittest.main()
