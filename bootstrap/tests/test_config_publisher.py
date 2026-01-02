import hashlib
import sys
import unittest
from pathlib import Path
from unittest import mock

BOOTSTRAP_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BOOTSTRAP_DIR / "config_publisher"))

import app  # noqa: E402


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

    def test_parse_objects_from_list(self) -> None:
        objects = app._parse_objects(
            {
                "Objects": [
                    {"Name": "spec", "Content": "openapi: 3.0.0"},
                    {
                        "Name": "config",
                        "Content": "listen_addr: 0.0.0.0:8080",
                        "Suffix": ".yml",
                        "ContentType": "text/plain",
                    },
                ]
            }
        )
        self.assertEqual([o.name for o in objects], ["spec", "config"])
        self.assertEqual(objects[0].suffix, ".yaml")
        self.assertEqual(objects[1].suffix, ".yml")
        self.assertEqual(objects[1].content_type, "text/plain")

    def test_parse_objects_empty_list_rejected(self) -> None:
        with self.assertRaises(ValueError):
            app._parse_objects({"Objects": []})

    def test_parse_objects_convenience_both(self) -> None:
        objects = app._parse_objects({"ConfigYaml": "a: 1", "SpecYaml": "openapi: 3.0.0"})
        self.assertEqual([o.name for o in objects], ["config", "spec"])

    def test_publish_uses_hashed_key(self) -> None:
        fake_s3 = _FakeS3()
        fake_boto3 = _FakeBoto3(fake_s3)

        obj = app.PublishObject(name="spec", content="hello")
        expected_sha = hashlib.sha256(b"hello").hexdigest()
        expected_key = f"lpr/spec/{expected_sha}.yaml"

        with mock.patch.object(app, "boto3", fake_boto3):
            data, physical_id = app._publish(bucket="my-bucket", prefix="lpr", objects=[obj])

        self.assertEqual(physical_id, "lpr-config-publisher:my-bucket:lpr/")
        self.assertEqual(data["BucketName"], "my-bucket")
        self.assertEqual(data["Published"]["spec"]["Key"], expected_key)
        self.assertEqual(data["Published"]["spec"]["Sha256"], expected_sha)
        self.assertEqual(data["Published"]["spec"]["S3Uri"], f"s3://my-bucket/{expected_key}")

        self.assertEqual(len(fake_s3.put_calls), 1)
        call = fake_s3.put_calls[0]
        self.assertEqual(call["Bucket"], "my-bucket")
        self.assertEqual(call["Key"], expected_key)
        self.assertEqual(call["Body"], b"hello")
        self.assertEqual(call["ContentType"], "text/yaml; charset=utf-8")
        self.assertEqual(call["Metadata"]["lpr-sha256"], expected_sha)
        self.assertEqual(call["Metadata"]["lpr-name"], "spec")

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
            "ResourceProperties": {"ConfigYaml": "a: 1"},
        }

        with mock.patch.object(app, "_cfn_send_response") as send:
            with mock.patch.dict(app.os.environ, {}, clear=True):
                app.handler(event, context=None)

        send.assert_called_once()
        kwargs = send.call_args.kwargs
        self.assertEqual(kwargs["status"], "FAILED")


if __name__ == "__main__":
    unittest.main()

