from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("gen_rdna2.py")
SPEC = """<Spec>
  <Document><Copyright>Synthetic test</Copyright><License>MIT</License><ReleaseDate>test</ReleaseDate></Document>
  <ISA>
    <Architecture><ArchitectureName>AMD RDNA 2</ArchitectureName></Architecture>
    <Encodings><Encoding>
      <EncodingName>ENC_TEST</EncodingName><BitCount>32</BitCount>
      <EncodingIdentifierMask>0</EncodingIdentifierMask>
      <EncodingIdentifiers><EncodingIdentifier>0</EncodingIdentifier></EncodingIdentifiers>
      <MicrocodeFormat><BitMap/></MicrocodeFormat>
    </Encoding></Encodings>
    <Instructions><Instruction>
      <InstructionName>S_TEST_BRANCH</InstructionName>
      <InstructionFlags>
        <IsBranch>{true}</IsBranch><IsConditionalBranch>TRUE</IsConditionalBranch>
        <IsProgramTerminator>{true}</IsProgramTerminator><IsImmediatelyExecuted>{false}</IsImmediatelyExecuted>
      </InstructionFlags>
      <InstructionEncodings><InstructionEncoding>
        <EncodingName>ENC_TEST</EncodingName><EncodingCondition>default</EncodingCondition>
        <Opcode>0</Opcode><Operands/>
      </InstructionEncoding></InstructionEncodings>
    </Instruction></Instructions>
  </ISA>
</Spec>"""


class InstructionFlagTests(unittest.TestCase):
    def test_xml_whitespace_does_not_change_instruction_flags(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "isa.xml"
            output = Path(tmp) / "spec.rs"

            def generate(true, false):
                source.write_text(SPEC.format(true=true, false=false), encoding="utf-8")
                subprocess.run([sys.executable, "-B", str(SCRIPT), str(source), str(output)], capture_output=True, check=True)
                return output.read_bytes()

            plain = generate("TRUE", "FALSE")
            self.assertIn(b'name: "S_TEST_BRANCH", flags: 11,', plain)
            self.assertEqual(generate(" \n true\t ", " false\n "), plain)


if __name__ == "__main__":
    unittest.main()
