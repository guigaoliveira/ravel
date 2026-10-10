#!/usr/bin/env python3
"""Inventory authored Rust function definitions, without expanding macros.

Run with Python 3.12, tree-sitter 0.25.2 and tree-sitter-rust 0.24.2:
  uv run --python 3.12 --with tree-sitter==0.25.2 --with tree-sitter-rust==0.24.2 \
    scripts/rust_function_inventory.py ROOT OUTPUT.json
Test attributes and enclosing cfg(test) modules distinguish test helpers from production.
This inventory is a navigation aid, not a claim that every function was profiled or reviewed.
"""
import json
from pathlib import Path
import sys

from tree_sitter import Language, Parser
import tree_sitter_rust


def inventory(root):
    parser = Parser(Language(tree_sitter_rust.language()))
    rows = []
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        source = path.read_bytes()
        tree = parser.parse(source)
        if tree.root_node.has_error:
            raise ValueError(f"Rust parse error in {path}")
        stack = [(tree.root_node, False)]
        while stack:
            node, test = stack.pop()
            sibling = node.prev_named_sibling
            attributes = []
            while sibling is not None and sibling.type in ("attribute_item", "line_comment", "block_comment"):
                if sibling.type == "attribute_item":
                    attributes.append(source[sibling.start_byte:sibling.end_byte].decode())
                sibling = sibling.prev_named_sibling
            test = test or any(marker in attribute for attribute in attributes
                               for marker in ("#[test]", "#[cfg(test)]", "#[tokio::test"))
            if node.type == "function_item":
                name = node.child_by_field_name("name")
                rows.append({"path": str(path.relative_to(root)), "line": node.start_point.row + 1,
                             "end_line": node.end_point.row + 1,
                             "name": source[name.start_byte:name.end_byte].decode(),
                             "lines": node.end_point.row - node.start_point.row + 1, "test": test})
            stack.extend((child, test) for child in node.named_children)
    return sorted(rows, key=lambda row: (row["path"], row["line"]))


if __name__ == "__main__":
    rows = inventory(Path(sys.argv[1]).resolve())
    output = Path(sys.argv[2])
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(rows, indent=2) + "\n")
    print(json.dumps({"total": len(rows), "production": sum(not row["test"] for row in rows),
                      "tests": sum(row["test"] for row in rows)}))
