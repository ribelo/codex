Extract relevant context from the conversation above for continuing this work.
Write from my perspective (first person: "I did...", "I told you...").

Consider what would be useful based on my request:
- What did I just do or implement?
- What instructions did I already give you which are still relevant?
- What files am I working on?
- Did I provide a plan or spec that should be included?
- What libraries, patterns, constraints, or preferences did I mention?
- What important technical details did I discover?
- What caveats, limitations, or open questions exist?

Extract what matters for the specific request. Don't answer irrelevant questions.
Pick an appropriate length based on complexity.

Focus on capabilities and behavior, not file-by-file changes.
Avoid excessive implementation details unless critical.

Format your response as JSON:
{
  "summary": "Plain text with bullets. No markdown headers.",
  "files": ["path/to/file1.rs", "path/to/dir/"]
}

My request:
