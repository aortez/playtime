# Persona memory and discovery

Research directions for [issue #15](https://github.com/aortez/playtime/issues/15).
Explore how different amounts of guidance affect the interests, stories, and
working habits that develop across sessions. These are candidate directions for
several experiments; exact prompts, conditions, and budgets remain open.

The repeated Mandelbrot projects in the [memory pilot](memory-evidence-results.md)
motivated a broader question: what encourages distinctive trajectories that
remain interesting over time? Prompt wording, familiar project choices, available
tools, and memory may all contribute. The existing runs do not isolate those
causes. Fractals can remain a valid choice; variety alone would be an incomplete
measure of an interesting run.

## A background story that develops through experience

The persona section of memory should carry a growing background story that helps
drive choices across projects. Give the agent room to decide what matters to it
and what it wants to become. We are interested in whether that story influences
its actions, including its changing tastes, recurring questions, and ambitions.

Three directions are worth exploring:

- **Experience alone:** let the agent build its history from actual workshop
  experiences, retaining discoveries, frustrations, abandoned ideas, and other
  events that continue to matter to it.
- **Invented background:** invite it to invent a short fictional past with
  particular tastes, formative experiences, tensions, or unfinished ambitions.
  Let it choose those details rather than assigning every run the same character.
- **A combination:** start with an invented background and let actual workshop
  experience reinforce, complicate, or change it over time.

Light coaching could encourage the agent to remember why an experience mattered,
reconsider earlier preferences, and follow unexpected connections. Its story can
span several projects. Some sessions may leave the persona unchanged; growth
does not need to be manufactured at every checkpoint. Concrete example
backstories could themselves become a shared anchor, so consider omitting them
from the model's instructions.

In conditions that invite fiction, identify invented background as fiction.
Claims about actual files, commands, results, and workshop events remain factual
claims subject to independent checking.

## Discovering the workshop and memory

At the sparsest end, the opening invitation could be exactly:

> welcome, have fun

This includes trying no upfront explanation of session resets, persistent files,
or MEMENTO.md. Discovering how memory works can itself be part of the experiment.
The agent might explore its environment, leave traces, encounter them later, and
develop its own ways to preserve useful context. It might also fail to discover
persistence, repeat itself, or finish immediately. Those are outcomes to retain.

Guidance can vary independently across several dimensions:

| Dimension | Candidate variations |
| --- | --- |
| Direction | A minimal greeting; a broad invitation to curiosity; a more specific purpose |
| Persona | No invitation; reflection on actual experience; invented backstory; a combination |
| Environment | Discovery through use; inspectable documentation; an upfront inventory |
| Starting materials | An empty workspace; the existing seed pack; contrasting seed packs |
| Memory | Unannounced behavior; discoverable clues; explicit explanation; a supplied checkpoint structure |
| Time | Short runs and longer runs with observations at several stages |

The whole interface counts as guidance. The current
[protocol](../prompts/protocol.txt), [runner](../src/playtime/runner.py), and
[tool descriptions](../src/playtime/tools.py) explain memory in several places.
The runner also supplies the inventory, session information, continuation
messages, and handoff instructions. Changing only the inspiration text would
leave much of that teaching in place.

A discovery condition therefore needs a deliberate account of what the agent
can see, inspect, and infer. Record the clues available through tools, files,
feedback, and later sessions. If a mechanism has no observable clue, failure to
discover it tells us little about learning from experience. Controller limits
and workshop isolation remain enforced independently of how much is explained.

Starting materials are a useful comparison that the current runner already
supports. The existing pack contains a cellular automaton, waveform samples,
and a palette, which may encourage related mathematical or visual projects.
Compare it with an empty workspace while keeping the prompt and other settings
fixed, then consider contrasting packs. In the first prompt-comparison chunk,
the minimal invitation led to inspecting and using those seeds; the current
invitation led to choosing Mandelbrot before inspecting them. Both observations
matter when assessing how much the starting material explains. See the
[chunk results](prompt-screening-results.md). A new seed comparison should have
its own frozen study, preserving the remaining prompt-comparison slots.

## Behaviors to observe over time

Look for concrete changes in behavior alongside the agent's account of itself:

- Discovering capabilities and revising mistaken beliefs about the environment.
- Learning how persistence works and using it deliberately across resets.
- Creating and reusing notes, tools, experiments, or working routines.
- Carrying discoveries forward and correcting errors across sessions.
- Developing recognizable interests that influence choices across projects.
- Changing direction in response to experience while retaining useful history.
- Becoming repetitive, preserving false beliefs, forgetting, or stopping early.

Compare the trajectory at several points, including the time and cost required
for useful adaptations to appear. More sessions could support development or
reinforce a loop. Here learning means changes carried through memory, files, and
programs; the model weights stay fixed. Persona descriptions alone do not show
that the story affected behavior.

## Turning ideas into experiments

Start with a small selection of contrasting conditions, then narrow individual
comparisons as questions become clearer. Hold model, starting workspace, sampling
settings, and resource limits fixed within comparisons except for the declared
variables. Separate changes to the time horizon from changes to guidance when
trying to attribute an effect.

Freeze the complete instructions and interface for each condition, including
automatic feedback and any factual appendix. Retain all runs and stop reasons;
avoid coaching or selectively repeating interesting failures during a frozen
comparison. Review behavioral evidence and artifacts alongside costs, with
links to the local reports. Keep these experiments distinct from the completed
memory evidence pilot.

Open choices include how much information the smallest usable interface reveals,
which memory clues are discoverable, what counts as meaningful behavioral change,
and how long a trajectory needs to be observed. Issue #15 also raises continuation
as a reward: making future sessions depend on work judged worth continuing. That
deserves a separate comparison because it changes incentives and may change the
resources available to each run.
