from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET


SCRIPT = Path(__file__).resolve().parent / "gen_rdna2.py"
# Original synthetic fixture: one encoding and instruction, no AMD XML copied.
SPEC = """<Spec>
<Document><Copyright>Synthetic fixture</Copyright><License>GPL-2.0-only</License>
<ReleaseDate>2026-11-26</ReleaseDate></Document>
<ISA><Architecture><ArchitectureName>AMD RDNA 2</ArchitectureName></Architecture>
<Encodings><Encoding Order="0"><EncodingName>ENC_TEST</EncodingName><BitCount>32</BitCount>
<EncodingIdentifierMask>11110000</EncodingIdentifierMask>
<EncodingIdentifiers><EncodingIdentifier>10100000</EncodingIdentifier></EncodingIdentifiers>
<MicrocodeFormat><BitMap><Field><FieldName>OP</FieldName><BitLayout>
<Range Order="0"><BitOffset>0</BitOffset><BitCount>4</BitCount></Range>
</BitLayout></Field></BitMap></MicrocodeFormat></Encoding></Encodings>
<Instructions><Instruction><InstructionName>SYNTHETIC_OP</InstructionName>
<InstructionFlags><IsProgramTerminator>TRUE</IsProgramTerminator></InstructionFlags>
<InstructionEncodings><InstructionEncoding><EncodingName>ENC_TEST</EncodingName>
<EncodingCondition>default</EncodingCondition><Opcode>3</Opcode><Operands/>
</InstructionEncoding></InstructionEncodings></Instruction></Instructions></ISA></Spec>"""


class ArchitectureTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        # Use the expected filename even for wrong metadata: the XML must decide.
        self.source = self.root / "amdgpu_isa_rdna2.xml"
        self.output = self.root / "rdna2_spec.rs"

    def invoke(self, spec):
        self.source.write_text(spec, encoding="utf-8")
        return subprocess.run([sys.executable, "-I", str(SCRIPT), str(self.source), str(self.output)],
                              capture_output=True, text=True)

    def test_wrong_architecture_preserves_existing_output(self):
        original = b"previous table\r\n\x00"
        for arch in ("AMD RDNA 1", "AMD RDNA 3", "AMD RDNA 3.5", "AMD RDNA 4", "AMD CDNA 2"):
            with self.subTest(architecture=arch):
                self.output.write_bytes(original)
                result = self.invoke(SPEC.replace("AMD RDNA 2", arch))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.output.read_bytes(), original)
                self.assertIn("AMD RDNA 2", result.stderr)
                self.assertIn(arch, result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    def test_wrong_architecture_does_not_create_output(self):
        result = self.invoke(SPEC.replace("AMD RDNA 2", "AMD RDNA 1"))
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.output.exists())

    def test_missing_or_empty_architecture_preserves_output(self):
        for missing in ("Architecture", "ArchitectureName", "text"):
            with self.subTest(missing=missing):
                root = ET.fromstring(SPEC)
                isa = root.find("ISA")
                arch = isa.find("Architecture")
                if missing == "Architecture":
                    isa.remove(arch)
                elif missing == "ArchitectureName":
                    arch.remove(arch.find("ArchitectureName"))
                else:
                    arch.find("ArchitectureName").text = ""
                self.output.write_bytes(b"keep")
                result = self.invoke(ET.tostring(root, encoding="unicode"))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.output.read_bytes(), b"keep")
                self.assertIn("AMD RDNA 2", result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    def test_missing_isa_rejected_without_traceback(self):
        for exists in (False, True):
            with self.subTest(output_exists=exists):
                if exists:
                    self.output.write_bytes(b"keep")
                result = self.invoke("<Spec><Document/></Spec>")
                self.assertEqual(result.returncode, 1)
                self.assertEqual(self.output.exists(), exists)
                if exists:
                    self.assertEqual(self.output.read_bytes(), b"keep")
                self.assertNotIn("Traceback", result.stderr)
                self.assertIn("expected AMD RDNA 2 specification, got None", result.stderr)

    def test_rdna2_still_generates_tables(self):
        result = self.invoke(SPEC)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 encodings (1 primary), 1 instructions", result.stdout)
        generated = self.output.read_text(encoding="utf-8")
        self.assertIn('pub const ARCH: &str = "AMD RDNA 2";', generated)
        self.assertIn('fixed_mask: 0xf0, fixed_value: 0xa0', generated)
        self.assertIn('name: "SYNTHETIC_OP", flags: 8, always_literal: false', generated)
        self.assertIn('encoding: 0, opcode: 3, operands: &[]', generated)


if __name__ == "__main__":
    unittest.main()
