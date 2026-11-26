from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("gen_tiling.py")


class TilingGeneratorTests(unittest.TestCase):
    def test_c_integer_literals_in_swizzle_masks(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            script = root / "scripts" / SCRIPT.name
            script.parent.mkdir()
            shutil.copyfile(SCRIPT, script)
            generated = root / "crates" / "xcl" / "src" / "gpu" / "tiling_tables.rs"
            generated.parent.mkdir(parents=True)

            def table(name, values):
                return f"const unsigned {name}[][8] = {{\n    {{{values}}}\n}};\n"

            header = "#define X0 1\n" + table("GFX10_SW_PATTERN_NIBBLE01", "010, 0X10, 16, 0x20, 0, X0 ^ 010, 0, 0")
            header += "".join(table(f"GFX10_SW_PATTERN_NIBBLE{i}", "0, 0, 0, 0") for i in (2, 3, 4))
            modes = (
                "256_S", "256_D", "4K_S", "4K_D", "64K_S", "64K_D", "64K_S_T", "64K_D_T", "4K_S_X",
                "4K_D_X", "64K_Z_X_1xaa", "64K_S_X", "64K_D_X", "64K_R_X_1xaa", "4K_S3", "64K_S3",
                "64K_S3_T", "4K_S3_X", "64K_S3_X", "64K_D3_X",
            )
            header += "".join(table(f"GFX10_SW_{mode}_RBPLUS_PATINFO", "0, 0, 0, 0, 0") for mode in modes)
            source = root / "gfx10SwizzlePattern.h"
            source.write_text(header, encoding="utf-8")

            result = subprocess.run([sys.executable, "-B", str(script), str(source)], capture_output=True, text=True)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("[0x8, 0x10, 0x10, 0x20, 0x0, 0x9, 0x0, 0x0]", generated.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
