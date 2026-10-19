/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * The aml.load payload: QEMU q35's DSDT (tests/corpus/acpi/qemu-q35/dsdt.dat),
 * moved under \KBNS so it loads beside any namespace. Made by mkssdt.py from
 * `iasl -d` output; ssdt-q35.aml is `iasl -p ssdt-q35 ssdt-q35.dsl`.
 */

DefinitionBlock ("", "SSDT", 1, "ABYSS", "KBENCH", 0x00000001)
{
    Device (\KBNS)
    {
        Name (_ADR, Zero)
    }

    Device (\KBNS.KSB)
    {
        Name (_ADR, Zero)
    }

    Device (\KBNS.KGPE)
    {
        Name (_ADR, Zero)
    }

    Scope (\KBNS)
    {
        OperationRegion (DBG, SystemIO, 0x0402, One)
        Field (DBG, ByteAcc, NoLock, Preserve)
        {
            DBGB,   8
        }

        Method (DBUG, 1, NotSerialized)
        {
            ToHexString (Arg0, Local0)
            ToBuffer (Local0, Local0)
            Local1 = (SizeOf (Local0) - One)
            Local2 = Zero
            While ((Local2 < Local1))
            {
                DBGB = DerefOf (Local0 [Local2])
                Local2++
            }

            DBGB = 0x0A
        }
    }

    Scope (\KBNS.KSB)
    {
        Device (PCI0)
        {
            Name (_HID, EisaId ("PNP0A08") )  
            Name (_CID, EisaId ("PNP0A03") )  
            Name (_UID, Zero)  
            Method (_OSC, 4, NotSerialized)  
            {
                CreateDWordField (Arg3, Zero, CDW1)
                If ((Arg0 == ToUUID ("33db4d5b-1ff7-401c-9657-7441c03dd766") ))
                {
                    CreateDWordField (Arg3, 0x04, CDW2)
                    CreateDWordField (Arg3, 0x08, CDW3)
                    Local0 = CDW3 
                    Local0 &= 0x1E
                    If ((Arg1 != One))
                    {
                        CDW1 |= 0x08
                    }

                    If ((CDW3 != Local0))
                    {
                        CDW1 |= 0x10
                    }

                    CDW3 = Local0
                }
                Else
                {
                    CDW1 |= 0x04
                }

                Return (Arg3)
            }

            Method (EDSM, 5, Serialized)
            {
                If ((Arg2 == Zero))
                {
                    Local0 = Buffer (One)
                        {
                             0x00                                             
                        }
                    If ((Arg0 != ToUUID ("e5c937d0-3553-4d7a-9117-ea4d19c3434d") ))
                    {
                        Return (Local0)
                    }

                    If ((Arg1 < 0x02))
                    {
                        Return (Local0)
                    }

                    Local0 [Zero] = 0x81
                    Return (Local0)
                }

                If ((Arg2 == 0x07))
                {
                    Local0 = Package (0x02)
                        {
                            Zero, 
                            ""
                        }
                    Local1 = DerefOf (Arg4 [Zero])
                    Local0 [Zero] = Local1
                    Return (Local0)
                }
            }
        }

        Device (DRAC)
        {
            Name (_HID, "PNP0C01" )  
            Name (_CRS, ResourceTemplate ()  
            {
                DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                    0x00000000,         
                    0xE0000000,         
                    0xEFFFFFFF,         
                    0x00000000,         
                    0x10000000,         
                    ,, , AddressRangeMemory, TypeStatic)
            })
        }
    }

    Scope (\KBNS.KSB.PCI0)
    {
        OperationRegion (PCST, SystemIO, 0x0CC0, 0x08)
        Field (PCST, DWordAcc, NoLock, WriteAsZeros)
        {
            PCIU,   32, 
            PCID,   32
        }

        OperationRegion (SEJ, SystemIO, 0x0CC8, 0x04)
        Field (SEJ, DWordAcc, NoLock, WriteAsZeros)
        {
            B0EJ,   32
        }

        OperationRegion (BNMR, SystemIO, 0x0CD0, 0x08)
        Field (BNMR, DWordAcc, NoLock, WriteAsZeros)
        {
            BNUM,   32, 
            PIDX,   32
        }

        Mutex (BLCK, 0x00)
        Method (PCEJ, 2, NotSerialized)
        {
            Acquire (BLCK, 0xFFFF)
            BNUM = Arg0
            B0EJ = (One << Arg1)
            Release (BLCK)
            Return (Zero)
        }

        Method (AIDX, 2, NotSerialized)
        {
            Acquire (BLCK, 0xFFFF)
            BNUM = Arg0
            PIDX = (One << Arg1)
            Local0 = PIDX 
            Release (BLCK)
            Return (Local0)
        }

        Method (PDSM, 5, Serialized)
        {
            If ((Arg2 == Zero))
            {
                Local0 = Buffer (One)
                    {
                         0x00                                             
                    }
                If ((Arg0 != ToUUID ("e5c937d0-3553-4d7a-9117-ea4d19c3434d") ))
                {
                    Return (Local0)
                }

                If ((Arg1 < 0x02))
                {
                    Return (Local0)
                }

                Local1 = Zero
                Local2 = AIDX (DerefOf (Arg4 [Zero]), DerefOf (Arg4 [One]
                    ))
                If (!((Local2 == Zero) | (Local2 == 0xFFFFFFFF)))
                {
                    Local1 |= One
                    Local1 |= (One << 0x07)
                }

                Local0 [Zero] = Local1
                Return (Local0)
            }

            If ((Arg2 == 0x07))
            {
                Local2 = AIDX (DerefOf (Arg4 [Zero]), DerefOf (Arg4 [One]
                    ))
                Local0 = Package (0x02){}
                If (!((Local2 == Zero) || (Local2 == 0xFFFFFFFF)))
                {
                    Local0 [Zero] = Local2
                    Local0 [One] = ""
                }

                Return (Local0)
            }
        }
    }

    Name (\KBNS.PICF, Zero)
    Method (\KBNS._PIC, 1, NotSerialized)  
    {
        PICF = Arg0
    }

    Scope (\KBNS.KSB)
    {
        Scope (PCI0)
        {
            Name (PRTP, Package (0x80)
            {
                Package (0x04)
                {
                    0xFFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0xFFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0xFFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0xFFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    Zero, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    One, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    0x02, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    0x03, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    Zero, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    One, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    0x02, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    0x03, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    Zero, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    One, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    0x02, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    0x03, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    Zero, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    One, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    0x02, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    0x03, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    Zero, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    One, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    0x02, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    0x03, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    Zero, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    One, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    0x02, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    0x03, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    Zero, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    One, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    0x02, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    0x03, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    Zero, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    One, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    0x02, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    0x03, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    Zero, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    One, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    0x02, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    0x03, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    Zero, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    One, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    0x02, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    0x03, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    Zero, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    One, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    0x02, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    0x03, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    Zero, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    One, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    0x02, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    0x03, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    Zero, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    One, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    0x02, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    0x03, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    Zero, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    One, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    0x02, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    0x03, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    Zero, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    One, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    0x02, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    0x03, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    Zero, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    One, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    0x02, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    0x03, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    Zero, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    One, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    0x02, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    0x03, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    Zero, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    One, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    0x02, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    0x03, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    Zero, 
                    LNKA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    One, 
                    LNKB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    0x02, 
                    LNKC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    0x03, 
                    LNKD, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    Zero, 
                    LNKA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    One, 
                    LNKB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    0x02, 
                    LNKC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    0x03, 
                    LNKD, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    Zero, 
                    LNKA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    One, 
                    LNKB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    0x02, 
                    LNKC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    0x03, 
                    LNKD, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    Zero, 
                    LNKA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    One, 
                    LNKB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    0x02, 
                    LNKC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    0x03, 
                    LNKD, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    Zero, 
                    LNKA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    One, 
                    LNKB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    0x02, 
                    LNKC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    0x03, 
                    LNKD, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    Zero, 
                    LNKE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    One, 
                    LNKF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    0x02, 
                    LNKG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    0x03, 
                    LNKH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    Zero, 
                    LNKA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    One, 
                    LNKB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    0x02, 
                    LNKC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    0x03, 
                    LNKD, 
                    Zero
                }
            })
            Name (PRTA, Package (0x80)
            {
                Package (0x04)
                {
                    0xFFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0xFFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0xFFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0xFFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    Zero, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    One, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    0x02, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0001FFFF, 
                    0x03, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    Zero, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    One, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    0x02, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0002FFFF, 
                    0x03, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    Zero, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    One, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    0x02, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0003FFFF, 
                    0x03, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0004FFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    Zero, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    One, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    0x02, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0005FFFF, 
                    0x03, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    Zero, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    One, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    0x02, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0006FFFF, 
                    0x03, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    Zero, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    One, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    0x02, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0007FFFF, 
                    0x03, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0008FFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    Zero, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    One, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    0x02, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0009FFFF, 
                    0x03, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    Zero, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    One, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    0x02, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000AFFFF, 
                    0x03, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    Zero, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    One, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    0x02, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000BFFFF, 
                    0x03, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000CFFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    Zero, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    One, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    0x02, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000DFFFF, 
                    0x03, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    Zero, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    One, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    0x02, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000EFFFF, 
                    0x03, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    Zero, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    One, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    0x02, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x000FFFFF, 
                    0x03, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0010FFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    Zero, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    One, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    0x02, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0011FFFF, 
                    0x03, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    Zero, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    One, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    0x02, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0012FFFF, 
                    0x03, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    Zero, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    One, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    0x02, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0013FFFF, 
                    0x03, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0014FFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    Zero, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    One, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    0x02, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0015FFFF, 
                    0x03, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    Zero, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    One, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    0x02, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0016FFFF, 
                    0x03, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    Zero, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    One, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    0x02, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0017FFFF, 
                    0x03, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0018FFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    Zero, 
                    GSIA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    One, 
                    GSIB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    0x02, 
                    GSIC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x0019FFFF, 
                    0x03, 
                    GSID, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    Zero, 
                    GSIA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    One, 
                    GSIB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    0x02, 
                    GSIC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001AFFFF, 
                    0x03, 
                    GSID, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    Zero, 
                    GSIA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    One, 
                    GSIB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    0x02, 
                    GSIC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001BFFFF, 
                    0x03, 
                    GSID, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    Zero, 
                    GSIA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    One, 
                    GSIB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    0x02, 
                    GSIC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001CFFFF, 
                    0x03, 
                    GSID, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    Zero, 
                    GSIA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    One, 
                    GSIB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    0x02, 
                    GSIC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001DFFFF, 
                    0x03, 
                    GSID, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    Zero, 
                    GSIE, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    One, 
                    GSIF, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    0x02, 
                    GSIG, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001EFFFF, 
                    0x03, 
                    GSIH, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    Zero, 
                    GSIA, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    One, 
                    GSIB, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    0x02, 
                    GSIC, 
                    Zero
                }, 

                Package (0x04)
                {
                    0x001FFFFF, 
                    0x03, 
                    GSID, 
                    Zero
                }
            })
            Method (_PRT, 0, NotSerialized)  
            {
                If ((PICF == Zero))
                {
                    Return (PRTP) 
                }
                Else
                {
                    Return (PRTA) 
                }
            }
        }

        Method (IQST, 1, NotSerialized)
        {
            If ((0x80 & Arg0))
            {
                Return (0x09)
            }

            Return (0x0B)
        }

        Method (IQCR, 1, Serialized)
        {
            Name (PRR0, ResourceTemplate ()
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, _Y00)
                {
                    0x00000000,
                }
            })
            CreateDWordField (PRR0, \KBNS.KSB.IQCR._Y00._INT, PRRI)  
            PRRI = (Arg0 & 0x0F)
            Return (PRR0) 
        }

        Device (LNKA)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, Zero)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQA))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQA |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQA))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQA = PRRI 
            }
        }

        Device (LNKB)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, One)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQB))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQB |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQB))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQB = PRRI 
            }
        }

        Device (LNKC)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x02)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQC))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQC |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQC))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQC = PRRI 
            }
        }

        Device (LNKD)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x03)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQD))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQD |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQD))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQD = PRRI 
            }
        }

        Device (LNKE)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x04)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQE))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQE |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQE))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQE = PRRI 
            }
        }

        Device (LNKF)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x05)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQF))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQF |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQF))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQF = PRRI 
            }
        }

        Device (LNKG)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x06)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQG))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQG |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQG))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQG = PRRI 
            }
        }

        Device (LNKH)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x07)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000005,
                    0x0000000A,
                    0x0000000B,
                }
            })
            Method (_STA, 0, NotSerialized)  
            {
                Return (IQST (PRQH))
            }

            Method (_DIS, 0, NotSerialized)  
            {
                PRQH |= 0x80
            }

            Method (_CRS, 0, NotSerialized)  
            {
                Return (IQCR (PRQH))
            }

            Method (_SRS, 1, NotSerialized)  
            {
                CreateDWordField (Arg0, 0x05, PRRI)
                PRQH = PRRI 
            }
        }

        Device (GSIA)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x10)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000010,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000010,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSIB)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x11)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000011,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000011,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSIC)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x12)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000012,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000012,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSID)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x13)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000013,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000013,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSIE)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x14)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000014,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000014,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSIF)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x15)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000015,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000015,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSIG)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x16)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000016,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000016,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }

        Device (GSIH)
        {
            Name (_HID, EisaId ("PNP0C0F") )  
            Name (_UID, 0x17)  
            Name (_PRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000017,
                }
            })
            Name (_CRS, ResourceTemplate ()  
            {
                Interrupt (ResourceConsumer, Level, ActiveHigh, Shared, ,, )
                {
                    0x00000017,
                }
            })
            Method (_DIS, 0, NotSerialized)  
            {
            }

            Method (_SRS, 1, NotSerialized)  
            {
            }
        }
    }

    Scope (\KBNS.KSB)
    {
        Device (HPET)
        {
            Name (_HID, EisaId ("PNP0103") )  
            Name (_UID, Zero)  
            OperationRegion (HPTM, SystemMemory, 0xFED00000, 0x0400)
            Field (HPTM, DWordAcc, Lock, Preserve)
            {
                VEND,   32, 
                PRD,    32
            }

            Method (_STA, 0, NotSerialized)  
            {
                Local0 = VEND 
                Local1 = PRD 
                Local0 >>= 0x10
                If (((Local0 == Zero) || (Local0 == 0xFFFF)))
                {
                    Return (Zero)
                }

                If (((Local1 == Zero) || (Local1 > 0x05F5E100)))
                {
                    Return (Zero)
                }

                Return (0x0F)
            }

            Name (_CRS, ResourceTemplate ()  
            {
                Memory32Fixed (ReadOnly,
                    0xFED00000,         
                    0x00000400,         
                    )
            })
        }
    }

    Scope (\KBNS.KGPE)
    {
        Name (_HID, "ACPI0006" )  
    }

    Scope (\KBNS.KSB)
    {
        Device (\KBNS.KSB.PCI0.PRES)
        {
            Name (_HID, EisaId ("PNP0A06") )  
            Name (_UID, "CPU Hotplug resources")  
            Mutex (CPLK, 0x00)
            Name (_CRS, ResourceTemplate ()  
            {
                IO (Decode16,
                    0x0CD8,             
                    0x0CD8,             
                    0x01,               
                    0x0C,               
                    )
            })
            OperationRegion (PRST, SystemIO, 0x0CD8, 0x0C)
            Field (PRST, ByteAcc, NoLock, WriteAsZeros)
            {
                Offset (0x04), 
                CPEN,   1, 
                CINS,   1, 
                CRMV,   1, 
                CEJ0,   1, 
                CEJF,   1, 
                Offset (0x05), 
                CCMD,   8
            }

            Field (PRST, DWordAcc, NoLock, Preserve)
            {
                CSEL,   32, 
                Offset (0x08), 
                CDAT,   32
            }

            Method (XINI, 0, Serialized)  
            {
                CSEL = Zero
            }
        }

        Device (\KBNS.KSB.CPUS)
        {
            Name (_HID, "ACPI0010" )  
            Name (_CID, EisaId ("PNP0A05") )  
            Method (CTFY, 2, NotSerialized)
            {
                If ((Arg0 == Zero))
                {
                    Notify (C000, Arg1)
                }

                If ((Arg0 == One))
                {
                    Notify (C001, Arg1)
                }

                If ((Arg0 == 0x02))
                {
                    Notify (C002, Arg1)
                }

                If ((Arg0 == 0x03))
                {
                    Notify (C003, Arg1)
                }
            }

            Method (CSTA, 1, Serialized)
            {
                Acquire (\KBNS.KSB.PCI0.PRES.CPLK, 0xFFFF)
                \KBNS.KSB.PCI0.PRES.CSEL = Arg0
                Local0 = Zero
                If ((\KBNS.KSB.PCI0.PRES.CPEN == One))
                {
                    Local0 = 0x0F
                }

                Release (\KBNS.KSB.PCI0.PRES.CPLK)
                Return (Local0)
            }

            Method (CEJ0, 1, Serialized)
            {
                Acquire (\KBNS.KSB.PCI0.PRES.CPLK, 0xFFFF)
                \KBNS.KSB.PCI0.PRES.CSEL = Arg0
                \KBNS.KSB.PCI0.PRES.CEJ0 = One
                Release (\KBNS.KSB.PCI0.PRES.CPLK)
            }

            Method (CSCN, 0, Serialized)
            {
                Acquire (\KBNS.KSB.PCI0.PRES.CPLK, 0xFFFF)
                Name (CNEW, Package (0xFF){})
                Name (CEJL, Package (0xFF){})
                Local3 = Zero
                Local4 = One
                While ((Local4 == One))
                {
                    Local4 = Zero
                    Local0 = One
                    Local1 = Zero
                    Local5 = Zero
                    While (((Local0 == One) && (Local3 < 0x04)))
                    {
                        Local0 = Zero
                        \KBNS.KSB.PCI0.PRES.CSEL = Local3
                        \KBNS.KSB.PCI0.PRES.CCMD = Zero
                        If ((\KBNS.KSB.PCI0.PRES.CDAT < Local3))
                        {
                            Break
                        }

                        If (((Local1 == 0xFF) || (Local5 == 0xFF)))
                        {
                            Local4 = One
                            Break
                        }

                        Local3 = \KBNS.KSB.PCI0.PRES.CDAT
                        If ((\KBNS.KSB.PCI0.PRES.CINS == One))
                        {
                            CNEW [Local1] = Local3
                            Local1++
                            Local0 = One
                        }

                        If ((\KBNS.KSB.PCI0.PRES.CRMV == One))
                        {
                            CEJL [Local5] = Local3
                            Local5++
                            Local0 = One
                        }

                        Local3++
                    }

                    Local2 = Zero
                    While ((Local2 < Local1))
                    {
                        Local3 = DerefOf (CNEW [Local2])
                        CTFY (Local3, One)
                        Debug = Local3
                        \KBNS.KSB.PCI0.PRES.CSEL = Local3
                        \KBNS.KSB.PCI0.PRES.CINS = One
                        Local2++
                    }

                    Local2 = Zero
                    While ((Local2 < Local5))
                    {
                        Local3 = DerefOf (CEJL [Local2])
                        CTFY (Local3, 0x03)
                        \KBNS.KSB.PCI0.PRES.CSEL = Local3
                        \KBNS.KSB.PCI0.PRES.CRMV = One
                        Local2++
                    }
                }

                Release (\KBNS.KSB.PCI0.PRES.CPLK)
            }

            Method (COST, 4, Serialized)
            {
                Acquire (\KBNS.KSB.PCI0.PRES.CPLK, 0xFFFF)
                \KBNS.KSB.PCI0.PRES.CSEL = Arg0
                \KBNS.KSB.PCI0.PRES.CCMD = One
                \KBNS.KSB.PCI0.PRES.CDAT = Arg1
                \KBNS.KSB.PCI0.PRES.CCMD = 0x02
                \KBNS.KSB.PCI0.PRES.CDAT = Arg2
                Release (\KBNS.KSB.PCI0.PRES.CPLK)
            }

            Processor (C000, 0x00, 0x00000000, 0x00)
            {
                Method (_STA, 0, Serialized)  
                {
                    Return (CSTA (Zero))
                }

                Name (_MAT, Buffer (0x08)  
                {
                     0x00, 0x08, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00   
                })
                Method (_OST, 3, Serialized)  
                {
                    COST (Zero, Arg0, Arg1, Arg2)
                }
            }

            Processor (C001, 0x01, 0x00000000, 0x00)
            {
                Method (_STA, 0, Serialized)  
                {
                    Return (CSTA (One))
                }

                Name (_MAT, Buffer (0x08)  
                {
                     0x00, 0x08, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00   
                })
                Method (_EJ0, 1, NotSerialized)  
                {
                    CEJ0 (One)
                }

                Method (_OST, 3, Serialized)  
                {
                    COST (One, Arg0, Arg1, Arg2)
                }
            }

            Processor (C002, 0x02, 0x00000000, 0x00)
            {
                Method (_STA, 0, Serialized)  
                {
                    Return (CSTA (0x02))
                }

                Name (_MAT, Buffer (0x08)  
                {
                     0x00, 0x08, 0x02, 0x02, 0x01, 0x00, 0x00, 0x00   
                })
                Method (_EJ0, 1, NotSerialized)  
                {
                    CEJ0 (0x02)
                }

                Method (_OST, 3, Serialized)  
                {
                    COST (0x02, Arg0, Arg1, Arg2)
                }
            }

            Processor (C003, 0x03, 0x00000000, 0x00)
            {
                Method (_STA, 0, Serialized)  
                {
                    Return (CSTA (0x03))
                }

                Name (_MAT, Buffer (0x08)  
                {
                     0x00, 0x08, 0x03, 0x03, 0x01, 0x00, 0x00, 0x00   
                })
                Method (_EJ0, 1, NotSerialized)  
                {
                    CEJ0 (0x03)
                }

                Method (_OST, 3, Serialized)  
                {
                    COST (0x03, Arg0, Arg1, Arg2)
                }
            }
        }
    }

    Method (\KBNS.KGPE._E02, 0, NotSerialized)  
    {
        \KBNS.KSB.CPUS.CSCN ()
    }

    Scope (\KBNS.KSB.PCI0)
    {
        Name (_CRS, ResourceTemplate ()  
        {
            WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode,
                0x0000,             
                0x0000,             
                0x00FF,             
                0x0000,             
                0x0100,             
                ,, )
            IO (Decode16,
                0x0CF8,             
                0x0CF8,             
                0x01,               
                0x08,               
                )
            WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                0x0000,             
                0x0000,             
                0x0CF7,             
                0x0000,             
                0x0CF8,             
                ,, , TypeStatic, DenseTranslation)
            WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                0x0000,             
                0x0D00,             
                0xFFFF,             
                0x0000,             
                0xF300,             
                ,, , TypeStatic, DenseTranslation)
            DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, Cacheable, ReadWrite,
                0x00000000,         
                0x000A0000,         
                0x000BFFFF,         
                0x00000000,         
                0x00020000,         
                ,, , AddressRangeMemory, TypeStatic)
            DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                0x00000000,         
                0x40000000,         
                0xDFFFFFFF,         
                0x00000000,         
                0xA0000000,         
                ,, , AddressRangeMemory, TypeStatic)
            DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                0x00000000,         
                0xF0000000,         
                0xFEBFFFFF,         
                0x00000000,         
                0x0EC00000,         
                ,, , AddressRangeMemory, TypeStatic)
            QWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, Cacheable, ReadWrite,
                0x0000000000000000, 
                0x0000380000000000, 
                0x00003807FFFFFFFF, 
                0x0000000000000000, 
                0x0000000800000000, 
                ,, , AddressRangeMemory, TypeStatic)
        })
        Device (GPE0)
        {
            Name (_HID, "PNP0A06" )  
            Name (_UID, "GPE0 resources")  
            Name (_STA, 0x0B)  
            Name (_CRS, ResourceTemplate ()  
            {
                IO (Decode16,
                    0x0620,             
                    0x0620,             
                    0x01,               
                    0x10,               
                    )
            })
        }

        Device (PHPR)
        {
            Name (_HID, "PNP0A06" )  
            Name (_UID, "PCI Hotplug resources")  
            Name (_STA, 0x0B)  
            Name (_CRS, ResourceTemplate ()  
            {
                IO (Decode16,
                    0x0CC0,             
                    0x0CC0,             
                    0x01,               
                    0x18,               
                    )
            })
        }
    }

    Scope (\KBNS)
    {
        Name (XS3, Package (0x04)  
        {
            One, 
            One, 
            Zero, 
            Zero
        })
        Name (XS4, Package (0x04)  
        {
            0x02, 
            0x02, 
            Zero, 
            Zero
        })
        Name (XS5, Package (0x04)  
        {
            Zero, 
            Zero, 
            Zero, 
            Zero
        })
    }

    Scope (\KBNS.KSB.PCI0)
    {
        Device (FWCF)
        {
            Name (_HID, "QEMU0002")  
            Name (_STA, 0x0B)  
            Name (_CRS, ResourceTemplate ()  
            {
                IO (Decode16,
                    0x0510,             
                    0x0510,             
                    0x01,               
                    0x0C,               
                    )
            })
        }
    }

    Scope (\KBNS.KSB)
    {
        Scope (PCI0)
        {
            Device (S00)
            {
                Name (_ADR, Zero)  
            }

            Device (S08)
            {
                Name (_ADR, 0x00010000)  
                Method (_S1D, 0, NotSerialized)  
                {
                    Return (Zero)
                }

                Method (_S2D, 0, NotSerialized)  
                {
                    Return (Zero)
                }

                Method (_S3D, 0, NotSerialized)  
                {
                    Return (Zero)
                }
            }

            Device (S10)
            {
                Name (_ADR, 0x00020000)  
            }

            Device (S18)
            {
                Name (_ADR, 0x00030000)  
            }

            Device (SF8)
            {
                Name (_ADR, 0x001F0000)  
                OperationRegion (PIRQ, PCI_Config, 0x60, 0x0C)
                Scope (\KBNS.KSB)
                {
                    Field (PCI0.SF8.PIRQ, ByteAcc, NoLock, Preserve)
                    {
                        PRQA,   8, 
                        PRQB,   8, 
                        PRQC,   8, 
                        PRQD,   8, 
                        Offset (0x08), 
                        PRQE,   8, 
                        PRQF,   8, 
                        PRQG,   8, 
                        PRQH,   8
                    }
                }

                Device (KBD)
                {
                    Name (_HID, EisaId ("PNP0303") )  
                    Name (_STA, 0x0F)  
                    Name (_CRS, ResourceTemplate ()  
                    {
                        IO (Decode16,
                            0x0060,             
                            0x0060,             
                            0x01,               
                            0x01,               
                            )
                        IO (Decode16,
                            0x0064,             
                            0x0064,             
                            0x01,               
                            0x01,               
                            )
                        IRQNoFlags ()
                            {1}
                    })
                }

                Device (MOU)
                {
                    Name (_HID, EisaId ("PNP0F13") )  
                    Name (_STA, 0x0F)  
                    Name (_CRS, ResourceTemplate ()  
                    {
                        IRQNoFlags ()
                            {12}
                    })
                }

                Device (LPT1)
                {
                    Name (_HID, EisaId ("PNP0400") )  
                    Name (_UID, One)  
                    Name (_STA, 0x0F)  
                    Name (_CRS, ResourceTemplate ()  
                    {
                        IO (Decode16,
                            0x0378,             
                            0x0378,             
                            0x08,               
                            0x08,               
                            )
                        IRQNoFlags ()
                            {7}
                    })
                }

                Device (COM1)
                {
                    Name (_HID, EisaId ("PNP0501") )  
                    Name (_UID, One)  
                    Name (_STA, 0x0F)  
                    Name (_CRS, ResourceTemplate ()  
                    {
                        IO (Decode16,
                            0x03F8,             
                            0x03F8,             
                            0x00,               
                            0x08,               
                            )
                        IRQNoFlags ()
                            {4}
                    })
                }

                Device (RTC)
                {
                    Name (_HID, EisaId ("PNP0B00") )  
                    Name (_CRS, ResourceTemplate ()  
                    {
                        IO (Decode16,
                            0x0070,             
                            0x0070,             
                            0x01,               
                            0x08,               
                            )
                        IRQNoFlags ()
                            {8}
                    })
                }
            }

            Device (SFA)
            {
                Name (_ADR, 0x001F0002)  
            }

            Device (SFB)
            {
                Name (_ADR, 0x001F0003)  
            }
        }
    }

    Scope (\KBNS.KGPE)
    {
        Method (_E01, 0, NotSerialized)  
        {
        }
    }
}

