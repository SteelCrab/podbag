"""Unit tests for the decisions in mac.py that keep it from touching the user's data.

usage: python3 -m unittest discover -s dev/bench -p 'test_*.py'
"""

import importlib.util
import os
import unittest

spec = importlib.util.spec_from_file_location(
    "mac", os.path.join(os.path.dirname(os.path.abspath(__file__)), "mac.py")
)
mac = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mac)


class ImagePresence(unittest.TestCase):
    def test_an_inspected_image_is_present(self):
        self.assertEqual(mac.image_presence(0, ""), "present")

    def test_only_a_missing_image_message_means_absent(self):
        for stderr in [
            "Error response from daemon: No such image: docker.io/library/alpine:3.22",
            "Error: No such image: docker.io/library/alpine:3.22",
            "Error: docker.io/library/alpine:3.22: image not known",
        ]:
            self.assertEqual(mac.image_presence(1, stderr), "absent", stderr)

    def test_other_failures_are_unknown(self):
        for stderr in [
            "error during connect: Get \"http://docker/v1.47/images/x/json\": dial unix "
            "/Users/u/.docker/run/docker.sock: connect: no such file or directory",
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
            "context deadline exceeded",
            "",
        ]:
            self.assertEqual(mac.image_presence(1, stderr), "unknown", stderr)


class Labels(unittest.TestCase):
    def test_only_containers_with_the_label_are_ours(self):
        out = "aaa 1\nbbb <no value>\nccc \nddd 0\neee 1\n"
        self.assertEqual(mac.labelled_ids(out), ["aaa", "eee"])


class Colima(unittest.TestCase):
    def test_colima_uses_its_own_profile(self):
        tool = mac.TOOLS["colima"]
        self.assertIn("--profile", tool["up"][0])
        self.assertIn("--profile", tool["down"][0])
        self.assertIn("podbag-bench/docker.sock", tool["cli"][-1])


if __name__ == "__main__":
    unittest.main()
