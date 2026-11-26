from contextlib import redirect_stdout
from io import StringIO
import json
from pathlib import Path
import tempfile
import unittest

from coverage_gate import main


class SavedReportTests(unittest.TestCase):
    def test_run_json_accepts_byte_order_marks(self):
        counts = {"total": 1, "translated": 1, "unsupported": 0, "validator_failed": 0, "panicked": 0, "timeout": 0}
        report = json.dumps({"stages": {"CS": counts}})
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            sweep = Path(tmp) / "sweep.json"
            baseline.write_text(report, encoding="utf-8")
            for encoding in ("utf-8", "utf-8-sig", "utf-16"):
                with self.subTest(encoding=encoding), redirect_stdout(StringIO()) as out:
                    sweep.write_text(report, encoding=encoding)
                    self.assertEqual(main(["--baseline", str(baseline), "--run-json", str(sweep)]), 0)
                    self.assertIn("coverage gate: no regressions", out.getvalue())


if __name__ == "__main__":
    unittest.main()
