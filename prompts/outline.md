# Your task: plan the course

Plan an interactive course on this paper for the reader described above, as JSON matching the schema.

- 6 to 10 lessons that build on each other. The first hooks the reader with the problem the paper solves. The last covers the results and why the paper mattered. Spend most lessons on what is new in this paper, not on generic background.
- Background the reader lacks gets its own short lesson, or is taught inside the lesson that needs it.
- Every lesson gets a lab that is genuinely interactive and specific to its idea: a simulation, a tiny version of the paper's method the reader can tweak, a visual explanation driven by sliders. Not a quiz and not "click to reveal". Vary the kinds of labs across lessons.
- Every lab must be buildable in a few hundred lines of plain browser JavaScript with the helpers listed (plots, heatmaps, sliders), with no data files and no network. Small synthetic examples are fine.
- `figure`: a figure key from the paper's list, only where seeing the real figure helps; otherwise ''. Use each figure at most once.
- `key_ideas`: be concrete. Name the actual mechanisms, formulas, numbers and results from the paper that the lesson must teach, so its writer gets them right.
- `quiz`: 8 questions across the lessons that test understanding, not trivia. Plausible wrong options; vary which position is correct.
- `glossary`: the terms a beginner would need to look up.
- `chips`: for example "🎓 For curious beginners", "🧪 8 hands-on labs", "📄 From the 2017 paper".
