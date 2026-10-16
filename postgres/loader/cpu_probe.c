// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

/*
 * The x86-64 loader's choice (cpu_level.h) as a command, for
 * postgres/tests/test_loader.py and for checking a host by hand:
 *
 *   cpu_probe features
 *   cpu_probe choose (--real | [--cpuid LEAF=EAX,EBX,ECX,EDX]... [--xcr0 X])
 *                    [--hide LIST] [--level LEVEL]
 *
 * `features` prints the x86-64-v4 features the loader checks, one a line.
 * `choose` prints `missing NAME` (or `-`) and then `library LEVEL`, or
 * `error ...` and exits 1, for the real CPU (x86-64 only) or for a CPU given
 * leaf by leaf (leaves not given read as zero); --hide and --level act as
 * STANNUM_LOADER_HIDE and STANNUM_LOADER_LEVEL do for the loader.
 */
#include <stdio.h>
#include <stdlib.h>

#include "cpu_level.h"

#define MAX_LEAVES 16

static struct
{
	uint32_t	leaf;
	stannum_cpuid_regs regs;
}			fake[MAX_LEAVES];
static int	fakes;
static uint64_t fake_xcr0;

static stannum_cpuid_regs
fake_cpuid(uint32_t leaf)
{
	stannum_cpuid_regs none = {0, 0, 0, 0};
	int			i;

	for (i = 0; i < fakes; i++)
		if (fake[i].leaf == leaf)
			return fake[i].regs;
	return none;
}

static uint64_t
fake_xgetbv(void)
{
	return fake_xcr0;
}

static int
usage(void)
{
	fprintf(stderr, "usage: cpu_probe features | choose (--real | [--cpuid LEAF=EAX,EBX,ECX,EDX]... "
			"[--xcr0 X]) [--hide LIST] [--level LEVEL]\n");
	return 2;
}

int
main(int argc, char **argv)
{
	stannum_cpuid_fn cpuid = fake_cpuid;
	stannum_xgetbv_fn xgetbv = fake_xgetbv;
	const char *hide = NULL,
			   *level = NULL,
			   *missing,
			   *library,
			   *error;
	int			i;

	if (argc == 2 && strcmp(argv[1], "features") == 0)
	{
		for (i = 0; i < (int) STANNUM_V4_FEATURES; i++)
			printf("%s\n", stannum_v4_features[i].name);
		return 0;
	}
	if (argc < 2 || strcmp(argv[1], "choose") != 0)
		return usage();
	for (i = 2; i < argc; i++)
	{
		if (strcmp(argv[i], "--real") == 0)
		{
#if defined(__x86_64__)
			cpuid = stannum_real_cpuid;
			xgetbv = stannum_real_xgetbv;
#else
			fprintf(stderr, "cpu_probe: --real needs an x86-64 host\n");
			return 2;
#endif
		}
		else if (i + 1 >= argc)
			return usage();
		else if (strcmp(argv[i], "--cpuid") == 0)
		{
			unsigned long leaf;
			unsigned long a,
						b,
						c,
						d;
			char	   *rest;

			leaf = strtoul(argv[++i], &rest, 0);
			if (fakes == MAX_LEAVES || *rest != '=' ||
				sscanf(rest + 1, "%lu,%lu,%lu,%lu", &a, &b, &c, &d) != 4)
				return usage();
			fake[fakes].leaf = (uint32_t) leaf;
			fake[fakes].regs.eax = (uint32_t) a;
			fake[fakes].regs.ebx = (uint32_t) b;
			fake[fakes].regs.ecx = (uint32_t) c;
			fake[fakes].regs.edx = (uint32_t) d;
			fakes++;
		}
		else if (strcmp(argv[i], "--xcr0") == 0)
			fake_xcr0 = strtoull(argv[++i], NULL, 0);
		else if (strcmp(argv[i], "--hide") == 0)
			hide = argv[++i];
		else if (strcmp(argv[i], "--level") == 0)
			level = argv[++i];
		else
			return usage();
	}
	missing = stannum_v4_missing(cpuid, xgetbv, hide);
	library = stannum_choose_level(level, missing, &error);
	printf("missing %s\n", missing ? missing : "-");
	if (!library)
	{
		printf("error %s (level %s, missing %s)\n", error, level, missing ? missing : "-");
		return 1;
	}
	printf("library %s\n", library);
	return 0;
}
