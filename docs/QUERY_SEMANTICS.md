# Query semantics: where this server differs from FalkorDB C

This server follows openCypher. In a few places FalkorDB C (the Redis module,
4.x) returns different rows; queries ported from a C deployment should be
checked against the list below. Each item is covered by
`native_host/src/bin/filter_regression.rs` and was found by differential
fuzzing against FalkorDB C 4.22 and a brute-force openCypher oracle.

## Parallel edges produce one row each

Every relationship in a pattern is bound per edge, whether it is named,
anonymous, or never referenced again. With three edges from `a` to `b`:

| Query | This server | FalkorDB C |
|---|---|---|
| `MATCH (a)-->(b) RETURN count(*)` | 3 | 1 |
| `MATCH (a)-[r]->(b) RETURN count(*)` | 3 | 3 |
| `MATCH (a)--(b) RETURN count(*)` | 6 | 2 |

Write queries follow the same rows: `MATCH (a)-->(b) SET b.n = b.n + 1`
increments `b.n` once per edge. To act once per node pair, deduplicate first:
`MATCH (a)-->(b) WITH DISTINCT a, b SET ...`.

## Relationship uniqueness covers the whole MATCH clause

Within one `MATCH`, no physical edge is bound twice — across anonymous
relationships, variable-length relationships and comma-separated parts of the
pattern alike (`MATCH (a)-[*1..3]-(a)-[r]->(b)` cannot reuse `r` inside the
path). Separate `MATCH` clauses do not constrain each other:
`MATCH (a)-[r]->(b) MATCH (a)-[s]->(b)` may bind the same edge to `r` and `s`.

## `MATCH` after `OPTIONAL MATCH` needs `WITH`

`OPTIONAL MATCH (a)-[:Z]->(q) MATCH (a)-[:Y]->(b)` is refused with
"A WITH clause is required to introduce a MATCH clause after an OPTIONAL
MATCH", as in FalkorDB C. Write `OPTIONAL MATCH ... WITH * MATCH ...`.

## A node bound to null matches nothing

`OPTIONAL MATCH (n:Missing) WITH n MATCH (n) RETURN n` returns no rows (C
returns one row with `n = null`).

## Labels on a bound node in `OPTIONAL MATCH` are predicates

`MATCH (n) OPTIONAL MATCH (m)-->(k), (n:B)` null-pads `m` and `k` for every
`n` that is not a `:B` (C ignores the label).
