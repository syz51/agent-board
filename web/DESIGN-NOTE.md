# Design 1: Linear

A quiet, dense work tool in the spirit of Linear: a tinted sidebar with the content on an
inset panel, and lists that run edge to edge across it.

## Key choices

- **Layout**: a 224px sidebar (navigation, then every board with its open asks) and an
  inset content panel with a soft outline. Lists, the boards table and task groups run
  across the full panel width, with group bands and rows meeting its edges; text lines
  up with the page title through one gutter variable (`--gutter`).
- **Density**: 36px task rows, 32px group bands, agents as rows instead of cards (who and
  state, then device, repo, branch and last seen on one line, then task and handoff).
- **Type**: Inter at 13px body, 12px meta, 20px page titles; small, quiet headings.
- **Color and surfaces**: cool greys with one violet accent. No shadows on content; the
  panel's outline is the only elevation. Status colours stay, always with a shape and a
  word: board tallies spell out "3 done · 1 review".
- **Navigation**: the sidebar on wide screens; a top bar and bottom tabs on phones. Needs
  you shows one mark: its count, or a dot for unseen updates when nothing waits.
- **Round-6 findings**: autopilot switches write On or Off; board goals open in full on
  click, tap or key; empty coordinators read "No coordinator"; event marks show the state
  an event leaves; agent rows start alike whatever facts they have.

## What it trades away

- Small type and edge-to-edge rows favour scanning on a desktop over reading on a phone.
- Cards are gone from lists, so a row's extent shows only by its rules and hover.
- Titles are small; the page's identity relies on the sidebar's current item.
