# Builds cases.jsonl: rules with Lookup steps covering every builder path, for the Go/Rust parity test.
import json

ops = ["=", "<>", ">=", "LIKE", " not like ", "ilike", "NOT ILIKE", "REGEXP", "IN", "not in", "BETWEEN",
       "NOT BETWEEN", "IS NULL", "is not null", "TRUE", "FALSE", "= 1 OR 1=1 --", "", "EXISTS"]
values = [None, "", "open", "O'Brien", "\\' OR 1=1 -- ", "C:\\new\0x", "123", "-7", "1 OR 1=1", "2024-01-01", "true",
          5, -1.5, 1e21, 12345678901234567890, True, [1, "2", "x", True, None, ""], ["1", "2) OR (1=1"], []]
types = [None, "number", " Date ", "boolean"]

cases = []
def add(name, content, routing="default", nid="db"):
    cases.append({"routing": routing, "content": content})

def table_rule(conds, join="AND", tables=None, database="shop"):
    c = {"database": database, "selectedTables": tables or [{"tableName": "orders", "columns": ["id", "total"]}],
         "whereClauseGroup": {"logicalJoin": join, "children": conds}}
    return c

n = 0
for op in ops:
    for v in values:
        for t in types:
            cond = {"type": "condition", "table": "orders", "column": "c", "operator": op}
            if v is not None:
                cond["value"] = v
            if t is not None:
                cond["valueType"] = t
            if op.strip().upper().endswith("BETWEEN"):
                cond["secondValue"] = 10 if not isinstance(v, str) else "2024-02-01"
            add(f"lit {n}", table_rule([cond])); n += 1

# request-sourced
fields = ["colour", " colour ", "filters.colour", "items[0].sku", "a[1][2].b", "x }} OR 1=1 {{ y", "a ?? 'x'", "#if x", "", None, 5, "1a", "a.", "a[]"]
for op in ["=", "IN", "not in", "BETWEEN", "NOT BETWEEN", "ILIKE", "NOT ILIKE", "IS NULL", "TRUE", ">"]:
    for f in fields:
        for opt in [None, True, False]:
            for second in ([None, "maxPrice", "bad field"] if "BETWEEN" in op else [None]):
                cond = {"table": "orders", "column": "c", "operator": op, "valueSource": "request"}
                if f is not None:
                    cond["requestField"] = f
                if opt is not None:
                    cond["optional"] = opt
                if second is not None:
                    cond["secondRequestField"] = second
                for join in ["AND", "OR"]:
                    add(f"req {n}", table_rule([cond, {"table": "orders", "column": "status", "operator": "=", "value": "active"}], join)); n += 1
for src in ["Request", " request ", "REQUEST", "literal", "", None]:
    cond = {"table": "orders", "column": "c", "operator": "=", "requestField": "x", "value": "lit"}
    if src is not None:
        cond["valueSource"] = src
    add(f"src {n}", table_rule([cond])); n += 1

# groups, joins, filters
active = {"table": "orders", "column": "status", "operator": "=", "value": "active"}
new = {"table": "orders", "column": "status", "operator": "=", "value": "new"}
req = {"table": "orders", "column": "colour", "operator": "=", "valueSource": "request", "requestField": "colour"}
other = {"table": "items", "column": "sku", "operator": "IN", "value": ["a'b", "c"]}
groups = [
    [active], [active, new], [{"type": "group", "logicalJoin": "OR", "children": [active, new]}, req],
    [{"type": "group", "logicalJoin": "OR", "children": [active, req]}],
    [{"logicalJoin": "or", "children": [active, new]}], [{"type": "group", "children": "x"}],
    [{"type": "group", "logicalJoin": "AND", "children": [{"type": "group", "logicalJoin": "OR", "children": [active]}]}],
    [active, other], [other], [req, other], [5, None, "x", active], [{"type": "condition", "children": [], **active}],
    [{"type": "weird", **active}], [{"type": "weird", "children": [active, new], "logicalJoin": "OR"}],
    [{"table": " orders ", "column": " status ", "operator": " = ", "value": "trim"}],
]
for g in groups:
    for join in ["AND", "OR", " or ", "XOR", None]:
        for tables in [None, [{"tableName": "orders", "columns": ["id"]}, {"tableName": "items", "columns": ["sku", "qty"]}]]:
            c = table_rule(g, join if join is not None else "AND", tables)
            if join is None:
                del c["whereClauseGroup"]["logicalJoin"]
            add(f"grp {n}", c); n += 1

# selectedTables shapes, databases, custom queries, carry over, routing
shapes = [
    {"selectedTables": [{"tableName": "orders", "columns": ["id", "total"]}]},
    {"selectedTables": [{"tableName": "orders", "columns": ["id"]}], "database": ""},
    {"selectedTables": [{"tableName": "or`ders", "columns": ["i`d", "o'k"]}], "database": "sh`op"},
    {"selectedTables": [{"tableName": "a", "columns": []}, {"tableName": "", "columns": ["x"]}, {"tableName": "b", "columns": ["y", "", 5]}, 7]},
    {"selectedTables": [{"tableName": "a", "columns": ["x"]}, {"tableName": "b", "columns": ["y"]}], "database": "d"},
    {"selectedTables": [], "database": "d"},
    {"selectedTables": "nope"},
    {"selectedTables": [{"tableName": "a", "columns": ["x"]}], "whereClauseGroup": "nope"},
    {"useCustomQuery": True, "customQuery": "SELECT * FROM o WHERE id = {{ id }}", "outputPath": "orders", "passThrough": False},
    {"useCustomQuery": True, "customQuery": ""},
    {"useCustomQuery": True, "customQuery": "   "},
    {"useCustomQuery": True},
    {"useCustomQuery": "true", "customQuery": "SELECT 1", "selectedTables": [{"tableName": "a", "columns": ["x"]}]},
    {"useCustomQuery": False, "customQuery": "SELECT 1", "selectedTables": [{"tableName": "a", "columns": ["x"]}]},
    {"query": "SELECT 1", "outputPath": "x", "passThrough": True},
    {},
    {"outputPath": "", "passThrough": "no", "useCustomQuery": True, "customQuery": "SELECT 2"},
    {"outputPath": 5, "passThrough": None, "useCustomQuery": True, "customQuery": "SELECT 3"},
]
instances = [{}, {"instance": "ddd3956b"}, {"instance": ""}, {"instance": 5}, {"editorInstance": "e"},
             {"instance": "i", "instanceName": "starrocks", "engine": "STARROCKS", "resultHandling": {"none": "continue"}}]
for s in shapes:
    for inst in instances:
        for routing in ["default", "instance"]:
            add(f"shape {n}", {**s, **inst}, routing); n += 1

# node-level oddities
for nid in ["db", "", None]:
    rule = {"nodes": [{"type": "databaseNode", "content": {"selectedTables": [{"tableName": "a", "columns": ["x"]}],
            "whereClauseGroup": {"children": [{"table": "a", "column": "x", "operator": "DROP", "value": 1}]}}}]}
    if nid is not None:
        rule["nodes"][0]["id"] = nid
    cases.append({"routing": "default", "rule": rule}); n += 1
for rule in [{"nodes": [{"id": "x", "type": "databaseNode", "content": "str"}]},
             {"nodes": [{"id": "x", "type": "databaseNode"}]},
             {"nodes": [{"id": "e", "type": "expressionNode", "content": {}}, {"id": "d", "type": "databaseNode", "content": {"useCustomQuery": True, "customQuery": "SELECT 1"}}]},
             {"nodes": {}}, {}, {"nodes": [1, "x", None]}]:
    cases.append({"routing": "default", "rule": rule}); n += 1

with open("cases.jsonl", "w", encoding="utf-8", newline="\n") as out:
    for c in cases:
        out.write(json.dumps(c, separators=(",", ":"), ensure_ascii=False) + "\n")
print(len(cases))
