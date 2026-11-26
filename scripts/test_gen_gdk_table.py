from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("gen_gdk_table.py")


class SlotRangeTests(unittest.TestCase):
    def test_out_of_range_slot_does_not_replace_generated_table(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            script = root / "scripts" / SCRIPT.name
            script.parent.mkdir()
            shutil.copyfile(SCRIPT, script)
            output = root / "crates" / "xcl" / "src" / "hle" / "xgameruntime" / "gdk_table.rs"
            output.parent.mkdir(parents=True)
            source = root / "input.txt"
            row = 'NAME(Example, "00000000-0000-0000-0000-000000000001", "00000000-0000-0000-0000-000000000002", {})\n'
            command = [sys.executable, "-B", str(script), str(source)]

            source.write_text(row.format(255), encoding="utf-8")
            subprocess.run(command, capture_output=True, encoding="utf-8", check=True)
            valid = output.read_bytes()
            self.assertIn(b'("Example", 0x00000000000000000000000000000001, 0x00000000000000000000000000000002, 255)', valid)

            source.write_text(row.format(256), encoding="utf-8")
            invalid = subprocess.run(command, capture_output=True, encoding="utf-8")
            self.assertNotEqual(0, invalid.returncode)
            self.assertIn("Example: slot 256", invalid.stderr)
            self.assertEqual(valid, output.read_bytes())


if __name__ == "__main__":
    unittest.main()
