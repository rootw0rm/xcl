from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("gen_bcpack_victory_tables.py")
ENTRY_BYTES = 116
WIDTHS_OFFSET = 50


class Bc7DictionaryTests(unittest.TestCase):
    def test_width_copy_mismatch_leaves_generated_table_intact(self):
        blob = bytearray(77982)
        bc7 = len(blob) - 255 * ENTRY_BYTES
        first = bc7 + WIDTHS_OFFSET
        second = first + ENTRY_BYTES
        # Entry zero stores its first index where other entries repeat their widths.
        blob[first:first + 4] = bytes((4, 0, 2, 0))
        blob[second:second + 4] = bytes((3, 0, 3, 0))

        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "synthetic.bin"
            output = Path(tmp) / "tables.rs"
            command = [sys.executable, "-B", str(SCRIPT), str(source), str(output)]
            source.write_bytes(blob)
            subprocess.run(command, capture_output=True, check=True)
            valid = output.read_bytes()

            blob[second + 2] = 4
            source.write_bytes(blob)
            invalid = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(invalid.returncode, 0)
            self.assertIn("BC7 entry 1: repeated index widths", invalid.stderr)
            self.assertEqual(output.read_bytes(), valid)


if __name__ == "__main__":
    unittest.main()
