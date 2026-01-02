import unittest
import importlib.util
from pathlib import Path
from unittest import mock

BOOTSTRAP_DIR = Path(__file__).resolve().parents[1]

_APP_PATH = BOOTSTRAP_DIR / "config_publisher" / "app.py"
_SPEC = importlib.util.spec_from_file_location("config_publisher_app", _APP_PATH)
assert _SPEC and _SPEC.loader
app = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(app)  # type: ignore[union-attr]


class _FakeS3:
    def __init__(self) -> None:
        self.put_calls = []

    def put_object(self, **kwargs):
        self.put_calls.append(kwargs)


class _FakeBoto3:
    def __init__(self, s3: _FakeS3) -> None:
        self._s3 = s3

    def client(self, service_name: str):
        assert service_name == "s3"
        return self._s3


class ConfigPublisherTests(unittest.TestCase):
    def test_normalize_prefix(self) -> None:
        self.assertEqual(app._normalize_prefix(""), "")
        self.assertEqual(app._normalize_prefix("lpr"), "lpr/")
        self.assertEqual(app._normalize_prefix("lpr/"), "lpr/")
        self.assertEqual(app._normalize_prefix("/lpr"), "lpr/")
        self.assertEqual(app._normalize_prefix(" /lpr/ "), "lpr/")

    def test_parse_port(self) -> None:
        self.assertEqual(app._parse_port(None), 8080)
        self.assertEqual(app._parse_port(8080), 8080)
        self.assertEqual(app._parse_port("8080"), 8080)
        self.assertEqual(app._parse_port(" 8080 "), 8080)
        with self.assertRaises(ValueError):
            app._parse_port(True)
        with self.assertRaises(ValueError):
            app._parse_port("not-a-port")

    def test_publish_uses_hashed_key(self) -> None:
        fake_s3 = _FakeS3()
        fake_boto3 = _FakeBoto3(fake_s3)

        router_config = {"max_inflight_invocations": 1}
        spec = {"openapi": "3.0.0", "paths": {}}

        with mock.patch.object(app, "boto3", fake_boto3):
            data, physical_id = app._publish(
                bucket="my-bucket",
                prefix="lpr",
                router_config=router_config,
                spec=spec,
                port=8080,
            )

        self.assertEqual(physical_id, "lpr-config-publisher:my-bucket:lpr/")
        self.assertEqual(data["BucketName"], "my-bucket")
        self.assertTrue(data["SpecKey"].startswith("lpr/spec/"))
        self.assertTrue(data["SpecKey"].endswith(".json"))
        self.assertEqual(data["SpecS3Uri"], f"s3://my-bucket/{data['SpecKey']}")
        self.assertTrue(data["ConfigKey"].startswith("lpr/config/"))
        self.assertTrue(data["ConfigKey"].endswith(".json"))
        self.assertEqual(data["ConfigS3Uri"], f"s3://my-bucket/{data['ConfigKey']}")

        self.assertEqual(len(fake_s3.put_calls), 2)
        calls_by_key = {c["Key"]: c for c in fake_s3.put_calls}
        self.assertIn(data["SpecKey"], calls_by_key)
        self.assertIn(data["ConfigKey"], calls_by_key)

        spec_call = calls_by_key[data["SpecKey"]]
        self.assertEqual(spec_call["Bucket"], "my-bucket")
        self.assertEqual(spec_call["ContentType"], "application/json")
        self.assertEqual(spec_call["Metadata"]["lpr-name"], "spec")

        config_call = calls_by_key[data["ConfigKey"]]
        self.assertEqual(config_call["Bucket"], "my-bucket")
        self.assertEqual(config_call["ContentType"], "application/json")
        self.assertEqual(config_call["Metadata"]["lpr-name"], "config")

        self.assertIn("spec_path", app.json.loads(config_call["Body"].decode("utf-8")))
        self.assertIn("listen_addr", app.json.loads(config_call["Body"].decode("utf-8")))
        self.assertEqual(
            app.json.loads(config_call["Body"].decode("utf-8"))["spec_path"],
            data["SpecS3Uri"],
        )
        self.assertEqual(
            app.json.loads(config_call["Body"].decode("utf-8"))["listen_addr"],
            "0.0.0.0:8080",
        )

    def test_handler_delete_success(self) -> None:
        event = {
            "RequestType": "Delete",
            "ResponseURL": "https://example.com/cfn-response",
            "StackId": "stack",
            "RequestId": "req",
            "LogicalResourceId": "MyResource",
        }

        with mock.patch.object(app, "_cfn_send_response") as send:
            app.handler(event, context=None)

        send.assert_called_once()
        kwargs = send.call_args.kwargs
        self.assertEqual(kwargs["status"], "SUCCESS")
        self.assertEqual(kwargs["data"], {})

    def test_handler_missing_bucket_fails(self) -> None:
        event = {
            "RequestType": "Create",
            "ResponseURL": "https://example.com/cfn-response",
            "StackId": "stack",
            "RequestId": "req",
            "LogicalResourceId": "MyResource",
            "ResourceProperties": {"RouterConfig": {}, "Spec": {}},
        }

        with mock.patch.object(app, "_cfn_send_response") as send:
            with mock.patch.dict(app.os.environ, {}, clear=True):
                app.handler(event, context=None)

        send.assert_called_once()
        kwargs = send.call_args.kwargs
        self.assertEqual(kwargs["status"], "FAILED")


if __name__ == "__main__":
    unittest.main()
