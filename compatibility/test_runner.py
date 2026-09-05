import unittest
from run import classify, same_result


class ResultClassificationTests(unittest.TestCase):
    def setUp(self):
        self.ok = {"returncode": 0, "timeout": False, "stdout_base64": "NDIK", "stderr_base64": "", "files": {}}

    def test_known_gap_cannot_hide_runtime_crash_or_timeout(self):
        case = {"native": "mismatch"}
        for actual in [dict(self.ok, returncode=-11), dict(self.ok, timeout=True)]:
            self.assertEqual(classify(case, "native", self.ok, None, actual), "regression")

    def test_unsupported_requires_named_normal_compiler_rejection(self):
        case = {"native": "unsupported", "diagnostic": "No module named 'numpy'"}
        rejection = dict(self.ok, returncode=1, stdout_base64="", stderr=case["diagnostic"])
        self.assertEqual(classify(case, "native", self.ok, rejection, None), "known_gap")
        for changed in [dict(rejection, returncode=-11), dict(rejection, timeout=True), dict(rejection, stderr="linking failed")]:
            self.assertEqual(classify(case, "native", self.ok, changed, None), "regression")

    def test_unexpected_pass_requires_updating_manifest(self):
        self.assertEqual(classify({"native": "mismatch"}, "native", self.ok, None, self.ok), "unexpected_pass")

    def test_compatibility_never_inherits_native_gap_exemption(self):
        wrong = dict(self.ok, stdout_base64="MAo=")
        self.assertEqual(classify({"native": "mismatch"}, "compat", self.ok, None, wrong), "regression")

    def test_known_mismatch_only_accepts_recorded_wrong_output(self):
        case = {"native": "mismatch", "native_stdout": "0\n"}
        wrong = dict(self.ok, stdout_base64="MAo=")
        self.assertEqual(classify(case, "native", self.ok, None, wrong), "known_gap")
        self.assertEqual(classify(case, "native", self.ok, None, dict(wrong, stdout_base64="MQo=")), "regression")

    def test_files_and_raw_bytes_are_part_of_parity(self):
        self.assertFalse(same_result(dict(self.ok, files={"output.csv": "changed"}), self.ok))
        self.assertFalse(same_result(dict(self.ok, stderr_base64="/w=="), self.ok))

    def test_failing_oracle_never_counts_as_compiler_pass(self):
        failed = dict(self.ok, returncode=1)
        self.assertEqual(classify({"native": "pass"}, "native", failed, None, failed), "oracle_error")


if __name__ == "__main__":
    unittest.main()
