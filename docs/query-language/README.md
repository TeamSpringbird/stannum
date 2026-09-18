# TINQL reference

These reference pages are adapted from [PlanetScale Lead's query-language
documentation](https://github.com/planetscale/lead/tree/3fcf441ac7c3d183de179b1f846ceb0ef83e1358/tinql/docs/src), originally authored by PlanetScale.
They are retained here for convenience, with SQL names adjusted for Stannum.
TINQL remains the language name; Stannum is the extension implementing it.
The inherited documentation is covered by the repository's [AGPL-3.0 license](../../LICENSE).

Start with the [introduction](introduction.md) for the basic syntax and examples.

- [Terms, wildcards, fuzzy matching, regex, and ranges](terms.md)
- [Boolean operators](boolean-operators.md)
- [Phrases](phrases.md)
- [Alternatives and minimum-match expressions](alternatives.md)
- [Proximity](proximity.md)
- [Span relations](span-relations.md)
- [Positional filters](positional-filters.md)
- [Boosts](boost.md)
- [Operator precedence](precedence.md)
- [Keywords and escaping](keywords.md)
- [Recipes](recipes.md)

These pages describe query syntax. See [Stannum's architecture](../architecture/segmented-storage.md)
for implementation limits and the [project README](../../README.md) for setup.
