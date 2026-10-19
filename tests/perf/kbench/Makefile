# SPDX-License-Identifier: BSD-2-Clause
# Built on the FreeBSD build server against the 15.1 sources:
#	make SYSDIR=/usr/src/sys
KMOD=	kbench
SRCS=	kbench.c opt_acpi.h bus_if.h device_if.h kb_ssdt.h
CLEANFILES+=	kb_ssdt.h

# The aml.load payload, as a byte array.
kb_ssdt.h: ${.CURDIR}/ssdt-q35.aml
	file2c 'static const unsigned char kb_ssdt[] = {' '};' < ${.ALLSRC} > ${.TARGET}

.include <bsd.kmod.mk>
