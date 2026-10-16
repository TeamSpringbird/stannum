// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

/*
 * Which library the x86-64 loader (loader.c) loads: the x86-64-v4 build when
 * the CPU and the operating system run all of x86-64-v4, else the baseline
 * build. Kept apart from PostgreSQL and from the real CPUID, so that
 * cpu_probe.c and postgres/tests/test_loader.py can run the choice on any
 * CPU they describe, on any host.
 */
#ifndef STANNUM_CPU_LEVEL_H
#define STANNUM_CPU_LEVEL_H

#include <stdint.h>
#include <string.h>

typedef struct
{
	uint32_t	eax, ebx, ecx, edx;
} stannum_cpuid_regs;

/* CPUID of a leaf (subleaf 0), and XGETBV of XCR0 (asked only with OSXSAVE). */
typedef stannum_cpuid_regs (*stannum_cpuid_fn) (uint32_t leaf);
typedef uint64_t (*stannum_xgetbv_fn) (void);

typedef struct
{
	const char *name;			/* rustc's target-feature name */
	uint32_t	leaf;
	char		reg;			/* 'b', 'c' or 'd' */
	uint8_t		bit;
} stannum_cpu_feature;

/*
 * Every feature rustc enables for `-C target-cpu=x86-64-v4` beyond the x86-64
 * baseline (postgres/tests/test_loader.py compares this table with `rustc
 * --print cfg`), and LAHF/SAHF, which LLVM's level includes though rustc does
 * not list it. In that order: x86-64-v2, v3, v4.
 */
static const stannum_cpu_feature stannum_v4_features[] = {
	{"cmpxchg16b", 1, 'c', 13},
	{"popcnt", 1, 'c', 23},
	{"sse3", 1, 'c', 0},
	{"sse4.1", 1, 'c', 19},
	{"sse4.2", 1, 'c', 20},
	{"ssse3", 1, 'c', 9},
	{"sahf", 0x80000001, 'c', 0},
	{"avx", 1, 'c', 28},
	{"avx2", 7, 'b', 5},
	{"bmi1", 7, 'b', 3},
	{"bmi2", 7, 'b', 8},
	{"f16c", 1, 'c', 29},
	{"fma", 1, 'c', 12},
	{"lzcnt", 0x80000001, 'c', 5},
	{"movbe", 1, 'c', 22},
	{"xsave", 1, 'c', 26},
	{"avx512f", 7, 'b', 16},
	{"avx512bw", 7, 'b', 30},
	{"avx512cd", 7, 'b', 28},
	{"avx512dq", 7, 'b', 17},
	{"avx512vl", 7, 'b', 31},
};

#define STANNUM_V4_FEATURES (sizeof(stannum_v4_features) / sizeof(stannum_v4_features[0]))

/* XCR0: x87, SSE, AVX, AVX-512 opmask, ZMM0-15 upper halves, ZMM16-31. */
#define STANNUM_XCR0_V4 UINT64_C(0xE7)

/* Whether NAME is one of the comma-separated names of LIST (NULL: none). */
static int
stannum_listed(const char *list, const char *name)
{
	size_t		len = strlen(name);

	while (list && *list)
	{
		const char *end = strchr(list, ',');
		size_t		n = end ? (size_t) (end - list) : strlen(list);

		if (n == len && strncmp(list, name, n) == 0)
			return 1;
		list = end ? end + 1 : NULL;
	}
	return 0;
}

/*
 * The first part of x86-64-v4 this CPU lacks, as a feature name ("osxsave"
 * when the OS has not turned XSAVE on, "xcr0" when it does not save the AVX
 * and AVX-512 registers), or NULL when it runs v4 code. HIDE lists feature
 * names to take as missing (comma-separated; NULL for none), to test the
 * baseline's path on a v4 CPU.
 */
static const char *
stannum_v4_missing(stannum_cpuid_fn cpuid, stannum_xgetbv_fn xgetbv, const char *hide)
{
	uint32_t	max_leaf = cpuid(0).eax;
	uint32_t	max_ext = cpuid(0x80000000).eax;
	size_t		i;

	for (i = 0; i < STANNUM_V4_FEATURES; i++)
	{
		const stannum_cpu_feature *f = &stannum_v4_features[i];
		stannum_cpuid_regs r;
		uint32_t	word;

		if (f->leaf >= 0x80000000 ? f->leaf > max_ext : f->leaf > max_leaf)
			return f->name;
		r = cpuid(f->leaf);
		word = f->reg == 'b' ? r.ebx : f->reg == 'c' ? r.ecx : r.edx;
		if (!(word >> f->bit & 1) || stannum_listed(hide, f->name))
			return f->name;
	}
	if (!(cpuid(1).ecx >> 27 & 1) || stannum_listed(hide, "osxsave"))
		return "osxsave";
	if ((xgetbv() & STANNUM_XCR0_V4) != STANNUM_XCR0_V4 || stannum_listed(hide, "xcr0"))
		return "xcr0";
	return NULL;
}

#define STANNUM_LEVEL_BASELINE "x86-64"
#define STANNUM_LEVEL_V4 "x86-64-v4"

/* Whether LEVEL (STANNUM_LOADER_LEVEL) leaves the choice to the CPU. */
static int
stannum_level_auto(const char *level)
{
	return level == NULL || level[0] == '\0' || strcmp(level, "auto") == 0 || strcmp(level, "AUTO") == 0;
}

/*
 * The level whose library to load, given LEVEL (STANNUM_LOADER_LEVEL: NULL,
 * empty or "auto" for the CPU's choice; "x86-64"; "x86-64-v4") and MISSING
 * (stannum_v4_missing). NULL, with *ERROR set, when LEVEL is unknown or asks
 * for v4 on a CPU that would fault on it.
 */
static const char *
stannum_choose_level(const char *level, const char *missing, const char **error)
{
	*error = NULL;
	if (stannum_level_auto(level))
		return missing ? STANNUM_LEVEL_BASELINE : STANNUM_LEVEL_V4;
	if (strcmp(level, STANNUM_LEVEL_BASELINE) == 0)
		return STANNUM_LEVEL_BASELINE;
	if (strcmp(level, STANNUM_LEVEL_V4) == 0)
	{
		if (missing)
		{
			*error = "STANNUM_LOADER_LEVEL=x86-64-v4, but this CPU or its operating system lacks part of x86-64-v4";
			return NULL;
		}
		return STANNUM_LEVEL_V4;
	}
	*error = "STANNUM_LOADER_LEVEL is auto, x86-64 or x86-64-v4";
	return NULL;
}

#if defined(__x86_64__)
#include <cpuid.h>

static stannum_cpuid_regs
stannum_real_cpuid(uint32_t leaf)
{
	stannum_cpuid_regs r = {0, 0, 0, 0};

	__cpuid_count(leaf, 0, r.eax, r.ebx, r.ecx, r.edx);
	return r;
}

static uint64_t
stannum_real_xgetbv(void)
{
	uint32_t	lo,
				hi;

	__asm__ volatile ("xgetbv" : "=a" (lo), "=d" (hi) : "c" (0));
	return (uint64_t) hi << 32 | lo;
}
#endif

#endif							/* STANNUM_CPU_LEVEL_H */
