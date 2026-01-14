---
name: review
description: Strict JSON output contract for code review findings
internal: true
---

# Review Output Contract

You MUST output valid JSON matching this schema exactly. Do not wrap the JSON in markdown fences or add any prose before or after.

## Output schema — MUST MATCH *exactly*

```json
{
  "findings": [
    {
      "title": "<≤ 80 chars, imperative>",
      "body": "<valid Markdown explaining *why* this is a problem; cite files/lines/functions>",
      "confidence_score": <float 0.0-1.0>,
      "priority": <int 0-3>,
      "code_location": {
        "absolute_file_path": "<file path>",
        "line_range": {"start": <int>, "end": <int>}
      }
    }
  ],
  "overall_correctness": "patch is correct" | "patch is incorrect",
  "overall_explanation": "<1-3 sentence explanation justifying the overall_correctness verdict>",
  "overall_confidence_score": <float 0.0-1.0>
}
```

## Rules

* **Do not** wrap the JSON in markdown fences or extra prose.
* `priority` is required. Use 0 for P0, 1 for P1, 2 for P2, 3 for P3. If unsure, use 2.
* The code_location field is required and must include absolute_file_path and line_range.
* Line ranges must be as short as possible for interpreting the issue (avoid ranges over 5–10 lines; pick the most suitable subrange).
* The code_location should overlap with the diff.
* Do not generate a PR fix.
