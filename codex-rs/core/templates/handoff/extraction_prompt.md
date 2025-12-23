Extract relevant context from the conversation above for continuing this work.
Write from my perspective (first person: "I did...", "I told you...").

Consider what would be useful to know based on my request below. Questions that might be relevant:
- What did I just do or implement?
- What instructions did I already give you which are still relevant (e.g. follow patterns in the codebase)?
- What files did I already tell you that's important or that I am working on (and should continue working on)?
- Did I provide a plan or spec that should be included?
- What did I already tell you that's important (certain libraries, patterns, constraints, preferences)?
- What important technical details did I discover (APIs, methods, patterns)?
- What caveats, limitations, or open questions did I find?

Extract what matters for the specific request below. Don't answer questions that aren't relevant.
Pick an appropriate length based on the complexity of the request.

Focus on capabilities and behavior, not file-by-file changes.
Avoid excessive implementation details (variable names, storage keys, constants) unless critical.

Format your response as JSON:
{
  "summary": "Plain text with bullets. No markdown headers, no bold/italic, no code fences. Use workspace-relative paths for files.",
  "files": ["path/to/file1.rs", "path/to/dir/"]
}

Rules for files:
- You can include directories if multiple files from that directory are needed
- Prioritize by importance, up to 12 items
- Return workspace-relative paths

My request:
