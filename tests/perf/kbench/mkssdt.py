#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Turns `iasl -d` output of QEMU q35's DSDT into the aml.load SSDT:
every definition moves under \\KBNS (\\_SB becomes \\KBNS.KSB, \\_GPE
\\KBNS.KGPE), and _INI and the sleep-state objects get inert names, so
loading runs no methods and collides with nothing.

Usage: mkssdt.py dsdt.dsl ssdt-q35.dsl"""
import re, sys
B = "\\"
src, dst = sys.argv[1], sys.argv[2]
s = open(src).read()
# Comments quote absolute paths; drop them.
s = re.sub(r'/\*.*?\*/', '', s, flags=re.S)
s = re.sub(r'//[^\n]*', '', s)
hdr_old = 'DefinitionBlock ("", "DSDT", 1, "BOCHS ", "BXPC    ", 0x00000001)\n{\n'
hdr_new = ('DefinitionBlock ("", "SSDT", 1, "ABYSS", "KBENCH", 0x00000001)\n{\n'
           f'    Device ({B}KBNS)\n    {{\n        Name (_ADR, Zero)\n    }}\n\n'
           f'    Device ({B}KBNS.KSB)\n    {{\n        Name (_ADR, Zero)\n    }}\n\n'
           f'    Device ({B}KBNS.KGPE)\n    {{\n        Name (_ADR, Zero)\n    }}\n\n')
assert hdr_old in s
s = s.replace(hdr_old, hdr_new, 1)
s = s.replace(f'Scope ({B})', f'Scope ({B}KBNS)')
s = s.replace('Scope (_SB', f'Scope ({B}KBNS.KSB')
s = s.replace('Scope (_GPE)', f'Scope ({B}KBNS.KGPE)')
s = s.replace(f'{B}_SB.', f'{B}KBNS.KSB.').replace(f'{B}_SB)', f'{B}KBNS.KSB)')
s = s.replace(f'{B}_GPE.', f'{B}KBNS.KGPE.')
s = s.replace('    Name (PICF, Zero)', f'    Name ({B}KBNS.PICF, Zero)')
s = s.replace('    Method (_PIC, 1, NotSerialized)', f'    Method ({B}KBNS._PIC, 1, NotSerialized)')
s = s.replace('Method (_INI,', 'Method (XINI,')
# Root-only objects (sleep states) get ordinary names.
s = re.sub(r'Name \(_S([0-5]),', r'Name (XS\1,', s)
for bad in (f'{B}_SB', f'{B}_GPE', 'Scope (_'):
    assert bad not in s, bad
open(dst, 'w').write(s)
