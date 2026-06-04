#!/usr/bin/env python3
"""Small report helpers for benchmark markdown/HTML/JSON artifacts."""

from __future__ import annotations

import html
import json
from pathlib import Path
from typing import Any


def table_column_class(label: str) -> str:
    normalized = " ".join(label.lower().strip().split())
    if normalized == "policy":
        return "col-policy"
    if normalized == "model":
        return "col-model"
    if normalized == "turns complete/requested":
        return "col-turns"
    if normalized == "cache hit":
        return "col-cache"
    if normalized in {"cost", "total cost"}:
        return "col-cost"
    if normalized in {"recovery toks", "recovery cost", "extra tools"}:
        return "col-recovery"
    if normalized in {
        "base llm reqs",
        "base reqs/turn",
        "expected llm reqs",
        "expected reqs/turn",
        "llm comp reqs",
        "llm comp items",
        "llm comp cost",
    }:
        return "col-llm"
    if normalized in {"tool struct comp", "tool saved toks"}:
        return "col-tool-comp"
    if normalized in {"failed at", "fail request", "limit fails"}:
        return "col-failure"
    return ""


def class_attr(css_class: str) -> str:
    return f' class="{html.escape(css_class)}"' if css_class else ""


def markdown_table_to_html(lines: list[str], start: int) -> tuple[str, int]:
    header = [cell.strip() for cell in lines[start].strip("|").split("|")]
    column_classes = [table_column_class(cell) for cell in header]
    is_definition_table = len(header) == 2 and header[1].lower() == "meaning"
    index = start + 2
    rows: list[list[str]] = []
    while index < len(lines) and lines[index].startswith("|"):
        rows.append([cell.strip() for cell in lines[index].strip("|").split("|")])
        index += 1

    table_class = " class=\"definition-table\"" if is_definition_table else ""
    parts = [f"<div class=\"table-wrap\"><table{table_class}>", "<thead><tr>"]
    for col_index, cell in enumerate(header):
        parts.append(f"<th{class_attr(column_classes[col_index])}>{html.escape(cell)}</th>")
    parts.append("</tr></thead><tbody>")
    for row in rows:
        parts.append("<tr>")
        for col_index, cell in enumerate(row):
            css_class = column_classes[col_index] if col_index < len(column_classes) else ""
            parts.append(f"<td{class_attr(css_class)}>{html.escape(cell)}</td>")
        parts.append("</tr>")
    parts.append("</tbody></table></div>")
    return "\n".join(parts), index


def markdown_to_html_body(markdown: str) -> str:
    lines = markdown.splitlines()
    parts: list[str] = []
    index = 0
    in_code = False
    code_lines: list[str] = []
    while index < len(lines):
        line = lines[index]
        if line.startswith("```"):
            if in_code:
                parts.append("<pre><code>{}</code></pre>".format(html.escape("\n".join(code_lines))))
                code_lines = []
                in_code = False
            else:
                in_code = True
            index += 1
            continue
        if in_code:
            code_lines.append(line)
            index += 1
            continue
        if line.startswith("|") and index + 1 < len(lines) and lines[index + 1].startswith("|---"):
            table_html, index = markdown_table_to_html(lines, index)
            parts.append(table_html)
            continue
        if line.startswith("# "):
            parts.append(f"<h1>{html.escape(line[2:].strip())}</h1>")
        elif line.startswith("## "):
            parts.append(f"<h2>{html.escape(line[3:].strip())}</h2>")
        elif line.startswith("### "):
            parts.append(f"<h3>{html.escape(line[4:].strip())}</h3>")
        elif line.startswith("- "):
            items = []
            while index < len(lines) and lines[index].startswith("- "):
                items.append(f"<li>{html.escape(lines[index][2:].strip())}</li>")
                index += 1
            parts.append("<ul>{}</ul>".format("\n".join(items)))
            continue
        elif line.strip():
            parts.append(f"<p>{html.escape(line.strip())}</p>")
        index += 1
    if in_code:
        parts.append("<pre><code>{}</code></pre>".format(html.escape("\n".join(code_lines))))
    return "\n".join(parts)


def render_html_report(title: str, markdown: str) -> str:
    body = markdown_to_html_body(markdown)
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{html.escape(title)}</title>
  <style>
    :root {{
      color-scheme: light;
      --bg: #f7f8fa;
      --panel: #ffffff;
      --text: #17202a;
      --muted: #5f6b7a;
      --line: #d8dee7;
      --head: #eef2f6;
      --accent: #0f766e;
      --accent-soft: #e6fffb;
      --warn-soft: #fff7ed;
    }}
    body {{
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      line-height: 1.45;
    }}
    main {{
      max-width: 1440px;
      margin: 0 auto;
      padding: 32px 24px 56px;
    }}
    h1, h2, h3 {{ line-height: 1.18; }}
    h1 {{ margin: 0 0 8px; font-size: 30px; }}
    h2 {{ margin-top: 32px; padding-top: 12px; border-top: 1px solid var(--line); }}
    h3 {{ margin-top: 24px; }}
    p, li {{ color: var(--muted); }}
    code {{
      background: #edf1f5;
      padding: 2px 5px;
      border-radius: 4px;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    }}
    pre {{
      background: #111827;
      color: #f9fafb;
      padding: 16px;
      border-radius: 8px;
      overflow: auto;
    }}
    .table-wrap {{
      overflow: auto;
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
      margin: 16px 0 28px;
      box-shadow: 0 1px 2px rgba(15, 23, 42, 0.05);
    }}
    table {{
      width: 100%;
      border-collapse: collapse;
      min-width: 1080px;
      font-size: 13px;
      font-variant-numeric: tabular-nums;
    }}
    table.definition-table {{
      min-width: 760px;
      table-layout: fixed;
    }}
    table.definition-table th,
    table.definition-table td {{
      text-align: left;
      white-space: normal;
      vertical-align: top;
      line-height: 1.5;
    }}
    table.definition-table th:first-child,
    table.definition-table td:first-child {{
      width: 240px;
      white-space: nowrap;
      font-weight: 700;
      color: var(--accent);
    }}
    th, td {{
      border-bottom: 1px solid var(--line);
      padding: 9px 10px;
      text-align: right;
      white-space: nowrap;
    }}
    th:first-child, td:first-child,
    th:nth-child(2), td:nth-child(2) {{
      text-align: left;
    }}
    th {{
      position: sticky;
      top: 0;
      background: var(--head);
      color: #253244;
      font-weight: 650;
      z-index: 1;
    }}
    tbody tr:nth-child(odd) td {{ background: #ffffff; }}
    tbody tr:nth-child(even) td {{ background: #f7f9fc; }}
    th.col-policy, td.col-policy {{
      position: sticky;
      left: 0;
      text-align: left;
      min-width: 210px;
      max-width: 260px;
      box-shadow: 1px 0 0 var(--line);
    }}
    th.col-policy {{
      z-index: 3;
      background: var(--head);
    }}
    td.col-policy {{
      z-index: 2;
      color: var(--accent);
      font-weight: 700;
      white-space: normal;
    }}
    th.col-model, td.col-model {{
      text-align: left;
      min-width: 140px;
      color: #334155;
    }}
    th.col-turns, td.col-turns {{
      font-weight: 650;
      color: #1f2937;
    }}
    th.col-cache, td.col-cache {{
      color: #166534;
      font-weight: 700;
    }}
    th.col-cost, td.col-cost {{
      background: var(--accent-soft);
      color: #0f172a;
      font-weight: 800;
    }}
    th.col-cost {{
      color: #0f766e;
    }}
    th.col-recovery, td.col-recovery {{
      background: var(--warn-soft);
      color: #7c2d12;
    }}
    th.col-tool-comp, td.col-tool-comp,
    th.col-llm, td.col-llm {{
      color: #475569;
    }}
    th.col-failure, td.col-failure {{
      color: #991b1b;
    }}
    tr:hover td {{ background: #eef6ff; }}
    tr:hover td.col-cost {{ background: #d8fbf5; }}
    tr:hover td.col-recovery {{ background: #ffedd5; }}
    tr:hover td.col-policy {{ background: #eef6ff; }}
    tr:last-child td {{ border-bottom: 0; }}
  </style>
</head>
<body>
<main>
{body}
</main>
</body>
</html>
"""


def write_report_files(
    output_dir: Path,
    markdown: str,
    html_text: str,
    json_payload: dict[str, Any],
    *,
    stem: str = "report",
) -> tuple[Path, Path, Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    markdown_path = output_dir / f"{stem}.md"
    html_path = output_dir / f"{stem}.html"
    json_path = output_dir / "results.json"
    markdown_path.write_text(markdown + "\n", encoding="utf-8")
    html_path.write_text(html_text, encoding="utf-8")
    json_path.write_text(
        json.dumps(json_payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    return markdown_path, html_path, json_path
