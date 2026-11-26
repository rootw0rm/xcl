from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "re" / "record_writers.py"


class RecordWriterTests(unittest.TestCase):
    def test_fallback_only_takes_reachable_calls(self):
        cases = (
            ("ret", "ret", "?"),
            ("repz ret", "ret", "?"),
            ("ud2", "ret", "?"),
            ("jmp 180001050 <other_block>", "ret", "?"),
            ("bnd jmp 180001050 <other_block>", "ret", "?"),
            ("jmp 180001050 <other_block>", "call *0x58(%rbx)", "11"),
            ("bnd jmp 180001050 <other_block>", "call *0x58(%rbx)", "11"),
            ("jne 0x180001050 <other_block>", "call *0x58(%rbx)", "11"),
            ("jmp 180001030 <fallback>", "call *0x58(%rbx)", "?"),
        )
        for branch, target, slot in cases:
            with self.subTest(branch=branch, target=target), tempfile.TemporaryDirectory() as tmp:
                listing = Path(tmp) / "synthetic.asm"
                fallthrough = "   180001032: ret\n" if branch.startswith("jne") else ""
                listing.write_text(
                    "   180001000: mov 0x18(%rcx),%rax\n"
                    "   180001004: cmp 0x30(%rcx),%rax\n"
                    "   180001008: jae 180001030 <fallback>\n"
                    "   18000100a: movl $0x1234,(%rax)\n"
                    "   180001010: add $0x20,%rax\n"
                    "   180001014: mov %rax,0x18(%rcx)\n"
                    f"   180001030: {branch}\n"
                    f"{fallthrough}"
                    "   180001040: call *0x40(%rbx)\n"
                    f"   180001050: {target}\n",
                    encoding="utf-8",
                )
                result = subprocess.run(
                    [sys.executable, "-B", str(SCRIPT), str(listing)],
                    capture_output=True, text=True, check=True,
                )
                self.assertIn(f"fallback slot {slot}", result.stdout)

    def test_branch_targets_with_and_without_hex_prefix(self):
        for target in ("180001030 <fallback>", "0x180001030 <fallback>"):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as tmp:
                listing = Path(tmp) / "synthetic.asm"
                listing.write_text(
                    "   180001000: mov 0x18(%rcx),%rax\n"
                    "   180001004: cmp 0x30(%rcx),%rax\n"
                    f"   180001008: jae {target}\n"
                    "   18000100a: movl $0x1234,(%rax)\n"
                    "   180001010: add $0x20,%rax\n"
                    "   180001014: mov %rax,0x18(%rcx)\n"
                    "   180001018: ret\n"
                    "   180001030: call *0x40(%rbx)\n",
                    encoding="utf-8",
                )
                result = subprocess.run(
                    [sys.executable, "-B", str(SCRIPT), str(listing)],
                    capture_output=True, text=True, check=True,
                )
                self.assertIn("1 record write sites", result.stdout)
                self.assertRegex(
                    result.stdout,
                    r"header imm 1234\s+length 32 B  bound \+0x30  fallback slot 8",
                )


if __name__ == "__main__":
    unittest.main()
