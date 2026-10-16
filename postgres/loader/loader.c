// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

/*
 * The x86-64 Linux loader: installed as stannum.so beside two builds of the
 * extension, stannum-x86-64.so (the baseline, which runs anywhere) and
 * stannum-x86-64-v4.so (compiled for x86-64-v4: AVX-512, BMI2, POPCNT and
 * the rest). PostgreSQL loads this file for every `$libdir/stannum` it
 * meets (CREATE EXTENSION, a function call, shared_preload_libraries, the
 * maintenance workers), checks its magic block and calls its _PG_init, which
 * picks a build by CPUID (cpu_level.h), opens it from this file's directory,
 * checks that its magic block matches this one, points this file's
 * trampolines at its symbols and runs its _PG_init.
 *
 * PostgreSQL then looks each SQL function's C symbol up in this file by
 * name, so this file exports every function symbol the builds export (but
 * _PG_init and Pg_magic_func), each a single indirect jump through a table
 * filled from the loaded build: the symbol list is generated from the
 * baseline build when the three are built together (script/install-release).
 * A jump preserves every argument register, so one trampoline serves any
 * signature (fmgr's V1 functions, their pg_finfo_ records, the background
 * workers' entry points). What a function returns into PostgreSQL (an index
 * access method's routine table, hooks, custom scan methods) points into the
 * build directly; only the SQL-level call goes through a trampoline.
 *
 * STANNUM_LOADER_LEVEL in the server's environment forces a build:
 * `x86-64`, or `x86-64-v4` (an error on a CPU without it, rather than
 * SIGILL); `auto` or unset chooses by CPU. STANNUM_LOADER_HIDE lists
 * feature names to take as missing, for tests. SHOW stannum.cpu_level reports
 * the level the running build was compiled for.
 *
 * Linked with -Bsymbolic (script/install-release), so that this file's own
 * references (Pg_magic_func) bind to its own definitions.
 */
#define _GNU_SOURCE
#include "postgres.h"
#include "fmgr.h"

#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "cpu_level.h"

#ifndef STANNUM_VERSION
#error "build with -DSTANNUM_VERSION=\"x.y.z\" (script/install-release)"
#endif

#ifdef PG_MODULE_MAGIC_EXT
PG_MODULE_MAGIC_EXT(.name = "stannum", .version = STANNUM_VERSION);
#else
PG_MODULE_MAGIC;
#endif

/* The builds' symbols: stannum_loader_symbols.h lists SYMBOL(index, name). */
static const char *const symbol_names[] = {
#define SYMBOL(index, name) [index] = #name,
#include "stannum_loader_symbols.h"
#undef SYMBOL
};

#define SYMBOLS (sizeof(symbol_names) / sizeof(symbol_names[0]))

/* Where each trampoline jumps: not_loaded until a build is loaded. */
__attribute__((visibility("hidden"))) void *stannum_loader_targets[SYMBOLS];

/*
 * One exported trampoline per symbol. endbr64 (a no-op without CET) keeps
 * them valid targets of indirect calls where indirect-branch tracking is on.
 */
#define SYMBOL(index, name) \
	__asm__(".text\n" \
			".globl " #name "\n" \
			".type " #name ", @function\n" \
			".p2align 4\n" \
			#name ":\n" \
			"\tendbr64\n" \
			"\tjmp *stannum_loader_targets+8*" #index "(%rip)\n" \
			".size " #name ", .-" #name "\n");
#include "stannum_loader_symbols.h"
#undef SYMBOL

/* What a trampoline runs before a build is loaded (never, as PostgreSQL
 * calls _PG_init first; this only makes a mistake an ERROR, not a crash). */
static void
not_loaded(void)
{
	ereport(ERROR,
			(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
			 errmsg("stannum: the loader has not loaded a build")));
}

__attribute__((constructor)) static void
init_targets(void)
{
	size_t		i;

	for (i = 0; i < SYMBOLS; i++)
		stannum_loader_targets[i] = (void *) not_loaded;
}

/*
 * palloc'd path of NAME in this file's directory. Found by a static
 * function's address: an exported one, such as _PG_init, could name another
 * extension's, which PostgreSQL loads into the global scope too.
 */
static char *
sibling(const char *name)
{
	Dl_info		info;
	const char *slash;

	if (dladdr((void *) sibling, &info) == 0 || info.dli_fname == NULL ||
		(slash = strrchr(info.dli_fname, '/')) == NULL)
		ereport(ERROR, (errmsg("stannum: the loader cannot find its own directory")));
	return psprintf("%.*s/%s", (int) (slash - info.dli_fname), info.dli_fname, name);
}

/* Whether a build's magic block matches this file's (same server ABI). */
static bool
same_abi(const Pg_magic_struct *mine, const Pg_magic_struct *theirs)
{
	if (theirs->len != mine->len)
		return false;
#ifdef PG_MODULE_MAGIC_EXT
	return memcmp(&mine->abi_fields, &theirs->abi_fields, sizeof(mine->abi_fields)) == 0;
#else
	return memcmp(mine, theirs, sizeof(*mine)) == 0;
#endif
}

void
_PG_init(void)
{
	const char *requested = getenv("STANNUM_LOADER_LEVEL");
	const char *missing = stannum_v4_missing(stannum_real_cpuid, stannum_real_xgetbv,
											 getenv("STANNUM_LOADER_HIDE"));
	const char *error;
	const char *level = stannum_choose_level(requested, missing, &error);
	char	   *path;
	void	   *build;
	PGModuleMagicFunction magic;
	void		(*init) (void);
	void	   *targets[SYMBOLS];
	size_t		i;

	if (level == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_PARAMETER_VALUE),
				 errmsg("stannum: %s", error),
				 errdetail("STANNUM_LOADER_LEVEL is \"%s\"; the CPU lacks %s.",
						   requested, missing ? missing : "nothing")));
	path = sibling(strcmp(level, STANNUM_LEVEL_V4) == 0 ? "stannum-x86-64-v4.so" : "stannum-x86-64.so");
	/* A v4 CPU chooses v4, but an install with the baseline alone still runs. */
	if (stannum_level_auto(requested) && strcmp(level, STANNUM_LEVEL_V4) == 0 && access(path, F_OK) != 0)
	{
		level = STANNUM_LEVEL_BASELINE;
		path = sibling("stannum-x86-64.so");
	}

	/* Local: its symbols must not stand in for this file's in later lookups. */
	build = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (build == NULL)
		ereport(ERROR,
				(errcode_for_file_access(),
				 errmsg("stannum: could not load \"%s\": %s", path, dlerror())));
	magic = (PGModuleMagicFunction) dlsym(build, PG_MAGIC_FUNCTION_NAME_STRING);
	if (magic == NULL || !same_abi(PG_MAGIC_FUNCTION_NAME(), magic()))
		ereport(ERROR,
				(errmsg("stannum: \"%s\" was built for another server than its loader", path),
				 errhint("Install the loader and both builds from one build (script/install-release).")));
	for (i = 0; i < SYMBOLS; i++)
	{
		targets[i] = dlsym(build, symbol_names[i]);
		if (targets[i] == NULL)
			ereport(ERROR,
					(errmsg("stannum: \"%s\" lacks the symbol \"%s\" its loader forwards", path,
							symbol_names[i]),
					 errhint("Install the loader and both builds from one build (script/install-release).")));
	}
	memcpy(stannum_loader_targets, targets, sizeof(targets));
	elog(DEBUG1, "stannum: loaded %s (%s%s%s)", path, level,
		 missing ? "; the CPU lacks " : "", missing ? missing : "");

	init = (void (*) (void)) dlsym(build, "_PG_init");
	if (init)
		init();
}
