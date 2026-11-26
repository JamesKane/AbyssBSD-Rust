/* SPDX-License-Identifier: MIT */
/*
 * What libdrm 2.4.134's meson build configures for FreeBSD 15.1 on arm64 (its meson.build), for
 * building the library's core by hand with cc: no udev, no PCI access library, no optional
 * drivers.
 */
#define UDEV 0
#define HAVE_SECURE_GETENV 0
#define HAVE_LIBDRM_ATOMIC_PRIMITIVES 1
#define HAVE_LIB_ATOMIC_OPS 0
#define HAVE_SYS_SYSCTL_H 1
#define HAVE_SYS_SELECT_H 1
#define HAVE_ALLOCA_H 0
#define HAVE_OPEN_MEMSTREAM 1
#define HAVE_VISIBILITY 1
#define HAVE_EXYNOS 0
#define HAVE_FREEDRENO_KGSL 0
#define HAVE_INTEL 0
#define HAVE_NOUVEAU 0
#define HAVE_RADEON 0
#define HAVE_VC4 0
#define HAVE_VMWGFX 0
#define HAVE_CAIRO 0
#define HAVE_VALGRIND 0
#define _GNU_SOURCE 1
