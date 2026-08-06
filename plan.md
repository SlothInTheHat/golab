# Atlas

> The multiplayer operating system for AI software engineering.

Version: MVP
Status: Living Document

---

# Vision

Software engineering is changing.

Developers are no longer writing every line of code themselves. Instead, every developer is beginning to work alongside one or more AI coding assistants capable of understanding repositories, implementing features, fixing bugs, and reviewing code.

While individual AI coding assistants have become remarkably capable, software engineering remains fundamentally a team activity. The largest bottleneck is no longer generating code—it is coordinating people and their agents as they work together.

Today, every AI coding assistant operates in isolation.

Claude Code understands your local repository.

Cursor understands your local repository.

OpenCode understands your local repository.

Your coworker's AI has no awareness of what your AI is doing.

The only synchronization point is Git.

By the time code reaches Git, duplicated work, merge conflicts, architectural drift, and unnecessary communication have already occurred.

Atlas exists to solve this problem.

Atlas transforms a Git repository into a live collaborative engineering workspace where humans and AI coding assistants share awareness before code is committed.

Rather than coordinating through pull requests, engineering teams coordinate through live engineering state.

---

# Elevator Pitch

Atlas is the multiplayer layer for AI software engineering.

Developers continue using their preferred coding tools while Atlas provides a shared understanding of the repository, current work, engineering goals, and live changes so humans and AI agents can collaborate in real time.

Think:

Figma for software engineering.

---

# What Atlas Is

Atlas is not another coding assistant.

Atlas does not generate code.

Atlas does not replace Git.

Atlas does not replace GitHub.

Atlas does not replace Cursor.

Atlas does not replace Claude Code.

Atlas is the coordination layer that allows multiple humans and multiple AI coding assistants to function as one engineering organization.

Git stores history.

Atlas manages the present.

---

# The Problem

Modern software engineering has evolved faster than the tooling.

A repository contains:

• source code

• architecture

• APIs

• documentation

• tests

• infrastructure

But it does not contain engineering state.

Questions like these require asking another person:

Who is working on this?

Is someone already changing this API?

Can I safely modify this function?

Why did this architecture change?

What should my agent work on next?

Which services are blocked?

What changed in the last five minutes?

Current tools cannot answer these questions.

Atlas can.

---

# Engineering State

Engineering state is everything happening around the code that Git cannot represent.

Examples include:

Current goals

Current tasks

Developers

AI agents

Current edits

Work in progress

Blocked work

Upcoming API changes

Reviews

Architecture decisions

Dependencies

Estimated completion

Repository activity

Engineering state changes continuously.

Atlas exists to maintain that state.

---

# Core Philosophy

The repository should feel alive.

Instead of becoming aware of changes after they have been committed, every participant should understand the project as it evolves.

Atlas should answer engineering questions before developers ask them.

Developers should spend less time coordinating work and more time building software.

---

# Guiding Principles

## 1. Atlas is infrastructure.

Atlas is not the place developers write code.

Atlas makes every existing coding environment better.

Developers continue using:

• Cursor

• Claude Code

• VS Code

• JetBrains

• OpenCode

Atlas quietly coordinates them.

---

## 2. Git stores history.

Atlas stores live engineering state.

Git answers:

"What happened?"

Atlas answers:

"What is happening?"

---

## 3. Humans remain in control.

Atlas coordinates work.

Humans decide direction.

AI agents execute.

Engineering leadership remains human.

---

## 4. Shared awareness is more valuable than automation.

Atlas should first make teams aware of what is happening.

Automation is only valuable once everyone shares the same understanding.

---

## 5. Coordination should disappear.

Developers should not think about Atlas while writing code.

Instead, Atlas quietly prevents duplicated work, surfaces important information, and keeps everyone synchronized.

The best coordination is invisible.

---

## 6. Never compete with the editor.

Atlas should never attempt to become another IDE.

Editors already solve editing extremely well.

Atlas should integrate with every editor rather than replacing them.

---

## 7. Every feature must reduce coordination cost.

If a feature does not reduce communication, duplicated work, waiting, or uncertainty, it probably does not belong in Atlas.

---

# Long-Term Vision

The future software engineering team consists of:

Several human developers.

Several AI coding assistants.

Testing agents.

Documentation agents.

Review agents.

Infrastructure agents.

All working simultaneously.

Atlas becomes the operating system coordinating this engineering organization.

Every participant shares the same engineering state.

Everyone knows:

Who is working.

What is changing.

Why it is changing.

What should happen next.

Software engineering becomes multiplayer.

Atlas is the platform that makes this possible.

---

# Success Criteria

Atlas succeeds when a team can answer these questions without asking another human:

Who is working on this feature?

Which AI agents are active?

Which APIs are currently changing?

What is blocked?

What should I work on next?

Who owns this service?

What changed recently?

Can I safely modify this code?

If Atlas can answer these questions instantly, it has achieved its purpose.

---

# The Atlas Experience

## Product Goal

Atlas should feel like opening a Figma document rather than cloning a Git repository.

When a developer joins a project they should immediately understand:

- what the team is building
- who is working
- what AI agents are doing
- what parts of the architecture are changing
- what they should work on next

without asking another human.

Atlas is not a dashboard.

Atlas is a shared engineering workspace.

---

# The Ideal Workflow

Today, software development looks like this:

Clone repository

↓

Open Slack

↓

Read Linear ticket

↓

Ask teammates what's happening

↓

Read Git history

↓

Open Pull Requests

↓

Open IDE

↓

Start coding

Most of the first thirty minutes are spent reconstructing context.

Atlas should reduce this to less than one minute.

---

# Opening a Workspace

A developer opens Cursor.

The Atlas extension automatically connects to the workspace.

No commands.

No configuration.

Within seconds Atlas knows:

• who joined

• which repository is open

• which branch is active

• which AI coding assistant is attached

The developer is immediately synchronized.

---

# The Workspace

Instead of opening into a file tree, Atlas presents a live engineering workspace.

------------------------------------------------

Refund Support

72% Complete

Contributors

Alice + Claude Code

Bob + Cursor

Charlie + Gemini

------------------------------------------------

Repository

Payments

Authentication

Orders

Frontend

Infrastructure

------------------------------------------------

Current Activity

Alice

Implementing Refund API

Bob

Updating Checkout UI

Charlie

Writing Tests

------------------------------------------------

Timeline

2:31 Alice started editing PaymentService

2:33 Refund endpoint changed

2:34 Frontend notified

------------------------------------------------

The developer understands the project in seconds.

---

# Choosing Work

Developers should never wonder:

"What should I work on?"

Atlas always knows what work is available.

When the developer presses:

Continue

Atlas determines:

Current goal

↓

Dependencies

↓

Current engineering state

↓

Available tasks

↓

Agent capabilities

↓

Safe work

Instead of manually assigning work, Atlas recommends the next task.

Humans can override recommendations at any time.

---

# Live Collaboration

This is Atlas's defining feature.

Suppose Alice begins modifying:

PaymentService

As soon as editing begins:

Bob immediately sees

PaymentService

Currently being modified

Owner

Alice

Agent

Claude Code

Progress

41%

ETA

4 minutes

No commits have been made.

The repository itself has not changed.

But everyone understands what is happening.

---

# The Engineering Overlay

Atlas introduces a concept called the Engineering Overlay.

The repository on disk represents code.

Atlas represents engineering state.

The overlay contains:

Current edits

Goals

Reviews

Notifications

Current owners

Upcoming API changes

Architecture decisions

Blocked work

Estimated completion

Engineering history

AI agents receive both:

Repository

+

Engineering Overlay

This allows them to reason about the future rather than only the present.

---

# Repository Exploration

Instead of browsing folders:

src/

components/

services/

controllers/

Developers browse architecture.

Application

↓

Payments

↓

Authentication

↓

Orders

↓

Frontend

↓

Infrastructure

Clicking Payments opens:

Current Goal

Current Workers

Current AI Agents

Recent Changes

Dependencies

Impact Radius

Open Reviews

Architecture Notes

The repository becomes understandable.

---

# Live Presence

Atlas should make engineering feel multiplayer.

Every participant appears automatically.

Alice

Cursor

Claude Code

Editing PaymentService

Progress

61%

------------------------------------------------

Bob

VS Code

Cursor Agent

Waiting for Payment API

------------------------------------------------

Charlie

JetBrains

Reviewing Tests

------------------------------------------------

Test Agent

Running Integration Tests

------------------------------------------------

Documentation Agent

Preparing API Documentation

The project feels alive.

---

# Notifications

Atlas should notify people about engineering events rather than Git events.

Examples:

Payment API changed.

Checkout depends on this.

Documentation required.

Review requested.

Architecture decision recorded.

Goal completed.

Notifications should answer:

Why do I care?

rather than simply stating what happened.

---

# Reviews

Completing work does not immediately release it.

Instead:

Developer

↓

Submit Review

↓

Reviewer notified

↓

Approved

↓

Engineering state updated

↓

Dependencies unblocked

↓

Other agents continue

The Engineering Graph remains trustworthy because completed work has passed review.

---

# Timeline

Every engineering event becomes part of a live timeline.

Examples:

2:31

Alice began implementing Refund API

2:32

Claude generated initial implementation

2:34

Return type changed

2:35

Checkout notified

2:36

Tests started

2:37

Review requested

2:40

Approved

The timeline should explain how the project evolved without reading Git commits.

---

# AI Collaboration

Atlas is not a chatbot.

Atlas coordinates autonomous workers.

Each AI coding assistant should continuously know:

Current goal

Current engineering state

Relevant architecture

Nearby work

Dependencies

Current blockers

Recent changes

Rather than working independently, AI agents become members of the engineering team.

---

# Human Collaboration

Humans remain responsible for:

Setting goals

Approving work

Making architectural decisions

Resolving conflicts

Prioritizing work

AI agents remain responsible for:

Implementing tasks

Writing tests

Updating documentation

Performing reviews

Refactoring

Atlas coordinates both equally.

---

# The Five-Minute Demo

A successful MVP should demonstrate the following:

1. Alice opens Cursor.

2. Bob opens Claude Code.

3. Both automatically appear in Atlas.

4. The Engineering Graph displays the repository.

5. Alice begins implementing Refund Support.

6. Bob immediately sees her progress.

7. Alice changes a public API.

8. Bob receives a notification before the change is committed.

9. Bob's agent automatically shifts to another task while waiting.

10. Alice requests review.

11. Bob approves.

12. The graph updates instantly.

13. The project advances without either developer manually coordinating work.

If this demo works, Atlas has achieved its MVP.

---

# The Engineering Graph

## The Heart of Atlas

The Engineering Graph is the central data model of Atlas.

Everything else in the system exists to either update it, query it, or visualize it.

Unlike a traditional knowledge graph, the Engineering Graph is not a static representation of source code.

It is a live model of the entire engineering organization.

It continuously combines:

- repository structure
- software architecture
- humans
- AI agents
- goals
- tasks
- engineering decisions
- current work
- reviews
- dependencies
- notifications
- activity

into a single shared representation.

The Engineering Graph becomes the source of truth for engineering coordination.

The Git repository remains the source of truth for source code.

---

# The Two Sources of Truth

Atlas intentionally separates code from engineering state.

## Git Repository

Git answers:

What code exists?

What changed?

Who committed it?

When?

Git stores history.

---

## Engineering Graph

The Engineering Graph answers:

What is happening?

Who is working?

What is changing?

What is blocked?

What will likely change next?

The Engineering Graph stores the present.

---

# Why This Matters

Today's coding assistants only understand code.

They have no understanding of work in progress.

For example:

PaymentService.cpp

may currently contain:

refundPayment()

returning

Payment

But Alice has already spent ten minutes rewriting it to return:

RefundResult

Her work has not been committed.

Every other developer and AI assistant still reasons about outdated information.

Atlas fills this gap.

The Engineering Graph records that:

PaymentService

↓

refundPayment()

↓

Currently Being Modified

↓

Expected Return Type

↓

RefundResult

↓

62% Complete

↓

Owner

Alice

↓

ETA

3 Minutes

The repository remains unchanged.

The Engineering Graph reflects reality.

---

# Graph Layers

Rather than representing only code, the Engineering Graph is composed of multiple interconnected layers.

## Code Layer

Contains structural information extracted from the repository.

Examples:

Repositories

Services

Directories

Files

Classes

Functions

Database Tables

Routes

Imports

Calls

Dependencies

This layer changes only when the repository changes.

---

## Architecture Layer

Groups code into concepts humans understand.

Instead of browsing folders:

src/

controllers/

models/

Developers browse:

Authentication

Payments

Orders

Checkout

Frontend

Infrastructure

This layer allows Atlas to explain software rather than merely index it.

---

## Human Layer

Represents every developer connected to the workspace.

Stores:

Current Goal

Current Task

Current Location

Recent Activity

Capabilities

Presence

Current Status

Working

Idle

Reviewing

Offline

---

## AI Layer

Represents every AI worker.

Examples:

Claude Code

Cursor Agent

OpenCode

Gemini

Local Models

Each AI has:

Current Context

Current Task

Current Goal

Current Progress

Current Status

Confidence

Capabilities

Recent Outputs

Rather than anonymous processes, AI agents become visible engineering participants.

---

## Goal Layer

Goals organize engineering work.

Goal

↓

Tasks

↓

Reviews

↓

Completion

↓

Dependencies

Goals are first-class citizens.

Every task belongs to a goal.

---

## Review Layer

Every completed task flows through review.

Task

↓

Submitted

↓

Review Requested

↓

Approved

↓

Released

↓

Dependencies Unblocked

This allows Atlas to distinguish between:

Implemented

and

Accepted.

---

## Activity Layer

Every engineering event updates the graph.

Examples:

Developer Joined

Goal Created

Task Started

Function Edited

API Changed

Review Requested

Review Approved

Goal Completed

Activity is permanent.

It becomes the engineering timeline.

---

# Engineering Overlay

The Engineering Graph is not intended to replace the repository.

Instead, Atlas creates an Engineering Overlay.

Repository

+

Engineering Graph

↓

Workspace State

This overlay represents everything that exists outside source code.

For example:

Alice

Currently editing PaymentService

Claude Code

Implementing OAuth

Review

Pending

Goal

Refund Support

ETA

2 minutes

All of this disappears once work is complete.

The repository contains only code.

Atlas contains engineering state.

---

# Querying the Graph

Every Atlas component interacts with the Engineering Graph.

## Dashboard

Visualizes it.

---

## IDE Extensions

Display relevant portions.

---

## AI Coding Assistants

Query it for context.

---

## Humans

Navigate it.

---

## Scheduler

Plans against it.

---

## Notification System

Subscribes to it.

Everything communicates through the same model.

---

# Agent Context

One of Atlas's primary responsibilities is providing better context to AI coding assistants.

Instead of simply exposing repository files, Atlas first provides engineering context.

Example:

Current Goal

Implement Refund Support

Current Workers

Alice

Payment API

Bob

Checkout UI

Recent Changes

PaymentService interface changing

Current Reviews

Authentication

Pending

Blocked Work

Documentation

Waiting on Payment API

Nearby Components

PaymentRepository

CheckoutController

WebhookHandler

Only after this context is delivered does the AI begin reading repository files.

The Engineering Graph augments code rather than replacing it.

---

# Real-Time Updates

Every significant engineering event updates the graph.

Examples include:

Developer joins

Developer leaves

Agent starts

Agent stops

Goal created

Goal completed

Task assigned

Task completed

Review requested

Review approved

File opened

Symbol edited

API changed

Architecture decision

Dependency discovered

The graph continuously evolves as the engineering organization evolves.

---

# The Digital Twin

Atlas should be thought of as maintaining a digital twin of the engineering organization.

At any moment it should be possible to answer:

Who is working?

Where?

On what?

Why?

With which AI?

How far along?

Who depends on them?

What will change next?

This is information Git cannot represent.

The Engineering Graph exists to make that information visible.

---

# Design Principles

The Engineering Graph must always satisfy these principles.

1. Repository code remains authoritative.

2. Engineering state augments code rather than replacing it.

3. Every update should improve shared awareness.

4. Humans and AI agents should consume the same engineering state.

5. The graph should describe the engineering organization rather than merely the repository.

6. Every visualization in Atlas should derive from the Engineering Graph.

If these principles remain true, every feature built on top of the graph will remain consistent.

---

# Definition of Done

This chapter is complete when Atlas can answer, in real time:

Who is working?

What are they working on?

Which AI is assisting them?

Which goals are active?

Which services are changing?

Which APIs are changing?

What reviews are pending?

What dependencies are blocked?

What happened recently?

What should happen next?

If Atlas can answer these questions through the Engineering Graph, the foundation of the platform is complete.

---

# Core Systems

Atlas is built around a small number of high-level concepts.

These concepts should be visible to users.

Everything else exists to support them.

The core systems are:

• Goals

• Engineering Graph

• Workers

• Reviews

• Timeline

• Notifications

These systems define how engineering work flows through Atlas.

---

# Goals

Goals are the highest-level unit of work.

Developers should think in terms of outcomes rather than tickets.

Examples:

Support Refunds

Implement OAuth

Improve Search Performance

Release Mobile App

A goal represents something valuable delivered to the project.

Every other object in Atlas ultimately belongs to a goal.

Goals contain:

- description
- priority
- current progress
- related architecture
- tasks
- reviews
- contributors
- AI workers

Goals answer:

Why are we doing this?

---

# Tasks

Tasks are scoped pieces of work that move a goal forward.

Unlike traditional issue trackers, Atlas should encourage tasks to remain small.

A task should ideally correspond to one engineering change.

Examples:

Implement refund endpoint

Update checkout UI

Write integration tests

Document webhook changes

Tasks should always be linked to symbols inside the Engineering Graph.

Rather than existing in isolation, Atlas knows exactly what parts of the repository a task affects.

---

# Workers

Atlas treats humans and AI agents equally.

Everything performing work is a worker.

Examples:

Alice

Bob

Claude Code

Cursor

Documentation Agent

Testing Agent

Workers have:

Current Goal

Current Task

Current Status

Current Context

Capabilities

Recent Activity

Progress

Rather than anonymous background processes, every worker is visible.

---

# Presence

Presence is one of Atlas's defining features.

Workers continuously broadcast:

Current location

Current symbol

Current task

Current goal

Progress

Status

Example:

Alice

Working

PaymentService

Refund API

Progress

42%

ETA

5 minutes

Presence should update continuously.

No refreshes.

---

# Current Work

Current work represents everything actively happening inside the repository.

Atlas continuously tracks:

Currently edited files

Currently edited symbols

Current reviews

Current discussions

Current blockers

Current dependencies

This information becomes part of the Engineering Graph.

---

# Reviews

Atlas assumes engineering work is not complete until reviewed.

Workflow:

Working

↓

Ready for Review

↓

Approved

↓

Released

↓

Dependencies Unblocked

Reviews protect the quality of the Engineering Graph.

Agents should never assume submitted work is accepted.

---

# Notifications

Notifications exist to reduce communication.

Every notification should answer:

"Why does this matter to me?"

Examples:

The API you depend on changed.

Your blocker has been resolved.

Your review is required.

A new goal matches your expertise.

A dependency is now available.

Notifications should never become noise.

---

# Timeline

Every engineering event appears in one shared timeline.

Examples:

Alice joined workspace.

Refund goal created.

Claude began PaymentService.

API changed.

Frontend notified.

Review requested.

Review approved.

Goal completed.

Timeline allows developers to understand project evolution without reading commits.

---

# Engineering State

Engineering state is Atlas's most important abstraction.

At any moment, Atlas should know:

Who is working.

Where they are working.

What they are trying to accomplish.

Who depends on them.

What is blocked.

What is changing.

What will likely happen next.

Engineering state continuously evolves.

The repository evolves much more slowly.

---

# The Work Loop

Every worker follows the same loop.

Join Workspace

↓

Receive Context

↓

Choose Goal

↓

Claim Task

↓

Perform Work

↓

Update Progress

↓

Submit Review

↓

Review Approved

↓

Next Task

This loop should feel natural regardless of whether the worker is human or AI.

---

# Human Workflow

Humans primarily:

Choose goals.

Review work.

Make architectural decisions.

Resolve ambiguity.

Coordinate priorities.

Humans steer engineering.

---

# AI Workflow

AI workers primarily:

Implement code.

Write tests.

Generate documentation.

Perform refactors.

Review changes.

Update progress.

AI executes engineering work.

---

# Shared Context

Every worker should always have access to:

Current Goal

Current Task

Nearby Architecture

Recent Changes

Current Workers

Open Reviews

Relevant Decisions

Dependencies

This shared context allows every participant to make better decisions.

---

# System Principles

Atlas should always satisfy the following principles.

Goals are more important than tasks.

Tasks are more important than files.

Files are more important than commits.

Commits are implementation details.

The engineering organization—not Git—is the primary abstraction.

---

# Definition of Done

The core systems are complete when:

Workers can join automatically.

Goals organize engineering work.

Tasks update live.

Presence is continuously visible.

Reviews gate completion.

Notifications reduce coordination.

Timeline explains project evolution.

Engineering state remains synchronized for every participant.

---

# Atlas Architecture

> Atlas is a real-time coordination platform that maintains a live Engineering Graph shared between humans, AI coding assistants, IDEs, and dashboards.

---

# High Level Architecture

```
                   Atlas Workspace

                    React Dashboard
                          │
                ┌─────────┴─────────┐
                │                   │
      VS Code Extension     Cursor Extension
                │                   │
         Claude Adapter      Cursor Adapter
                │                   │
                └─────────┬─────────┘
                          │
                 WebSocket / RPC API
                          │
                 Atlas Runtime Server
                          │
        ┌─────────────────┼─────────────────┐
        │                 │                 │
 Engineering Graph   Event Bus      Scheduler
        │                 │                 │
        └────────────┬────┴─────────────────┘
                     │
            Repository Watcher
                     │
              Local Git Repository
```

The runtime is the single source of truth.

Nothing communicates directly with anything else.

Every component communicates through the runtime.

---

# Core Components

Atlas consists of six major systems.

## 1. Runtime

The runtime is the heart of Atlas.

Responsibilities:

- maintain workspace state
- maintain Engineering Graph
- manage workers
- publish events
- manage reviews
- manage goals
- synchronize extensions
- synchronize dashboard

The runtime should never generate code.

It only coordinates engineering work.

---

## 2. Engineering Graph

The Engineering Graph represents live engineering state.

Unlike Git it stores:

- workers
- goals
- active edits
- architecture
- reviews
- notifications
- timeline
- predictions
- dependencies

Every subsystem reads and writes the graph.

The graph is Atlas.

---

## 3. Repository Watcher

The repository watcher continuously observes:

- filesystem changes
- git status
- branch changes
- tree-sitter symbols
- dependency graph
- imports
- API definitions

Whenever something changes it publishes an event.

The runtime updates the Engineering Graph.

---

## 4. Event Bus

Everything communicates through events.

Examples:

WorkerJoined

WorkerLeft

GoalCreated

TaskStarted

TaskCompleted

ReviewSubmitted

ReviewApproved

SymbolEditing

SymbolReleased

ApiChanged

ArchitectureChanged

PresenceUpdated

NotificationCreated

TimelineEvent

Every subsystem subscribes to events.

No subsystem communicates directly.

---

## 5. Dashboard

The dashboard is a visualization.

It never owns data.

Everything displayed comes from the Engineering Graph.

The dashboard subscribes to live updates over WebSockets.

No polling.

---

## 6. IDE Extensions

Every supported IDE has an extension.

Responsibilities:

- connect to runtime
- detect opened repository
- identify worker
- identify coding assistant
- display notifications
- display engineering graph
- stream edits
- provide context to agents

The extension should remain lightweight.

All intelligence lives inside Atlas.

---

# The Engineering Loop

The entire platform follows one continuous loop.

```
Repository Changes

↓

Repository Watcher

↓

Engineering Graph Updated

↓

Event Published

↓

Dashboard Updated

↓

Extensions Updated

↓

Agents Receive New Context

↓

Humans Continue Working

↓

Repository Changes
```

This loop should execute continuously.

---

# Worker Model

Atlas represents every participant as a worker.

Workers include:

Human Developer

Claude Code

Cursor

Gemini

OpenCode

Testing Agent

Documentation Agent

Review Agent

Every worker has:

id

name

type

status

goal

task

progress

current symbol

current repository

capabilities

presence

last heartbeat

Workers never communicate directly.

They communicate through Atlas.

---

# Presence

Every extension continuously streams presence.

Examples:

opened file

current symbol

cursor position (optional)

goal

task

progress

status

Presence should update approximately once per second.

Atlas uses presence to construct engineering state.

---

# Live Editing

Atlas should know what is changing before Git knows.

Example:

Alice edits

PaymentService.ts

Extension detects edit

↓

Event published

↓

Engineering Graph updated

↓

Bob notified

↓

Bob's agent receives updated context

↓

Bob avoids conflicting work

No commit is required.

---

# Goals

Goals organize engineering work.

Goals own:

tasks

reviews

workers

dependencies

architecture nodes

Goals are the primary navigation method.

---

# Reviews

Every task eventually enters review.

```
Working

↓

Submitted

↓

Approved

↓

Released

↓

Complete
```

Reviews update Engineering Graph state.

---

# Timeline

Every event is appended to the timeline.

Timeline is append-only.

Examples:

Alice joined

Claude started Payment API

Review requested

Checkout notified

Review approved

Goal completed

Timeline allows replaying engineering history.

---

# Notifications

Notifications are generated automatically.

Examples:

API changed

Dependency unblocked

Review requested

Goal completed

Architecture modified

Notifications are personalized.

Only relevant workers receive them.

---

# AI Context

One of Atlas's primary responsibilities is improving AI context.

Rather than sending only repository files, Atlas provides:

Current Goal

Current Task

Nearby Workers

Recent Changes

Architecture

Current Reviews

Dependencies

Relevant Timeline

Predicted Changes

The AI then reads repository files.

Engineering state is always preferred over stale repository state.

---

# Dashboard Screens

The MVP dashboard contains six screens.

Workspace

Engineering Graph

Goals

Timeline

Workers

Reviews

Everything else can wait.

---

# Extension Responsibilities

The extension should feel invisible.

Developer opens repository.

↓

Extension connects.

↓

Worker appears.

↓

Context synchronizes.

↓

Notifications appear.

↓

Engineering Graph updates.

↓

Developer continues coding.

No manual commands.

---

# Runtime Responsibilities

The runtime owns everything.

It is responsible for:

workspace

workers

graph

events

notifications

reviews

goals

presence

scheduler

No other component owns state.

---

# Design Rules

1. Repository code remains authoritative.

2. Engineering Graph owns engineering state.

3. Dashboard only visualizes.

4. Extensions only connect IDEs.

5. Runtime owns coordination.

6. AI agents consume Engineering Graph before repository files.

7. Every component communicates through events.

8. Humans remain in control.

---

# Definition of Done

The architecture is complete when:

Two developers open different editors.

Both appear automatically.

The Engineering Graph updates continuously.

The dashboard updates instantly.

Edits appear before commits.

Agents receive engineering context automatically.

Reviews synchronize correctly.

Goals progress automatically.

No developer needs to coordinate through Slack to avoid conflicting work.

---

# User Interface Specification

Atlas should make software engineering visible.

Rather than presenting files, commits, or tickets, Atlas presents the current state of the engineering organization.

The UI should answer one question immediately:

> **What is happening right now?**

Every screen exists to increase shared awareness.

---

# Design Principles

Atlas should feel calm.

It should avoid becoming another overwhelming developer dashboard.

Every screen should prioritize:

- live collaboration
- engineering context
- visual clarity
- minimal interaction
- real-time updates

Developers should spend most of their time inside their editor.

The Atlas UI should remain open on a second monitor or browser tab.

---

# Primary Layout

```
+--------------------------------------------------------------+
| Atlas                                       Connected ●      |
+--------------------------------------------------------------+

 Goals      Engineering Graph      Timeline      Reviews

---------------------------------------------------------------

                Engineering Graph

        ○ Payments ───── Checkout
         │                │
         │                │
 Authentication ───── Orders
         │
 Infrastructure

---------------------------------------------------------------

Workers

🟢 Alice      Claude Code      Refund API
🟢 Bob        Cursor           Checkout
🟡 Charlie    Reviewing
⚪ Test Agent Idle

---------------------------------------------------------------

Recent Activity

10:31 Alice began PaymentService

10:32 Checkout notified

10:33 Review requested
```

The Engineering Graph is always the center of the application.

---

# Screen 1 — Workspace

Purpose:

Provide an instant overview of the project.

Shows:

Current Goal

Overall Progress

Connected Workers

Current Reviews

Repository Health

Recent Activity

Open Notifications

A developer should understand the entire project within ten seconds.

---

# Screen 2 — Engineering Graph

This is Atlas's defining visualization.

Nodes represent:

Repositories

Services

Packages

Modules

Files (optional)

Goals

Workers

Reviews

Edges represent:

Calls

Imports

Dependencies

Current Work

Reviews

Ownership

Live Activity

Nodes should change color based on engineering state.

Green

Healthy

Yellow

Being Modified

Blue

Review

Red

Blocked

Gray

Inactive

---

# Live Activity

Nodes animate.

Examples:

Alice edits PaymentService

↓

PaymentService pulses yellow

↓

Nearby services briefly highlight

↓

Dependent workers receive notifications

↓

Timeline updates

Users should feel the repository evolving in real time.

---

# Screen 3 — Goals

Goals become the primary navigation system.

```
Refund Support

72%

███████████░░░░

Workers

Alice

Claude

Tasks

✓ API

▶ Checkout

○ Tests
```

Goals replace traditional issue lists.

---

# Screen 4 — Workers

Every participant appears automatically.

Example:

```
Alice

Human

Cursor

Working

Refund API

42%

-------------------------

Bob

Human

Claude Code

Waiting

Checkout

Blocked

-------------------------

Test Agent

Running

Integration Tests
```

Workers should never disappear unexpectedly.

Presence builds trust.

---

# Screen 5 — Timeline

The timeline explains the evolution of engineering work.

Example:

```
10:31

Alice started Refund API

10:32

Claude generated implementation

10:33

Payment API changed

10:33

Checkout notified

10:34

Bob switched tasks

10:36

Review requested
```

This is an engineering timeline.

Not a Git timeline.

---

# Screen 6 — Reviews

Reviews should become a first-class workflow.

```
Review Queue

Refund API

Reviewer

Bob

Status

Waiting

--------------------------------

Authentication Tests

Reviewer

Alice

Approved
```

Completing work should feel deliberate.

---

# Screen 7 — Notifications

Notifications are contextual.

Examples:

Payment API changed

A dependency has become available

Review requested

Worker disconnected

Architecture decision updated

Notifications should explain:

Why this matters.

---

# Repository View

Clicking a node opens an engineering view.

Example:

```
Payment Service

Workers

Alice

Claude

Goal

Refund Support

Dependencies

Checkout

Billing

Webhook

Recent Changes

Return type changing

Reviews

Pending

Timeline

Open
```

Developers should explore architecture rather than folders.

---

# Search

Universal search should find:

Goals

Workers

Files

Symbols

Services

Reviews

Timeline Events

Architecture

Typing:

payment

should immediately display everything related to payments.

---

# Live Presence

Every worker has a live avatar.

Hovering reveals:

Current Goal

Current Task

Current Symbol

Progress

Editor

Agent

ETA

Presence should feel effortless.

---

# Second Monitor Experience

Atlas is designed to remain visible throughout development.

Ideal setup:

```
+----------------------+----------------------+

Cursor                Atlas

Code                  Engineering Graph

                      Workers

                      Timeline

                      Reviews

                      Notifications

+----------------------+----------------------+
```

Developers rarely interact with Atlas.

They glance at it.

---

# Mobile Support

Not required for MVP.

The dashboard should be responsive enough for basic viewing.

No editing.

---

# Performance Goals

Dashboard loads in under two seconds.

Live updates appear in under 200 milliseconds.

Graph interactions remain smooth with thousands of nodes.

Scrolling timeline never blocks rendering.

---

# Design Language

Dark-first.

Modern.

Minimal.

Motion should communicate engineering activity rather than decoration.

Animations should reinforce collaboration.

Never distract.

---

# Definition of Done

The UI is complete when a new developer can answer the following questions without asking another person:

What is the team building?

Who is working?

Which AI agents are active?

What is changing?

What is blocked?

Which reviews are waiting?

What should I work on?

If the interface answers these questions within ten seconds of opening Atlas, the UI has achieved its goal.

---

# MVP Roadmap

> Build the smallest version of Atlas that proves the core idea:
>
> **Multiple developers using different AI coding assistants can collaborate on the same repository with shared awareness before code is committed.**

The MVP is **not** intended to solve every engineering problem.

Its purpose is to validate that shared engineering state provides meaningful value over Git alone.

---

# Success Criteria

The MVP is successful if two developers can:

- Open the same repository
- Connect automatically to Atlas
- See each other in real time
- See what the other is editing
- Receive notifications about relevant changes
- Avoid conflicting work
- Track progress through goals
- Complete a feature without manually coordinating through Slack

If those experiences feel significantly better than today's workflow, Atlas has proven its value.

---

# Milestone 1 — Atlas Runtime

**Goal:** Build the central coordination server.

### Features

- Atlas runtime
- Workspace management
- WebSocket server
- Repository registration
- Worker registration
- Presence system
- Event bus
- SQLite persistence
- Configuration system

### Deliverables

```
atlas dev

↓

Workspace starts

↓

Clients connect

↓

Workers appear
```

### Definition of Done

- Runtime launches locally
- Multiple clients connect
- Presence updates propagate
- Events broadcast in real time

---

# Milestone 2 — Engineering Graph

**Goal:** Build the live model of the repository.

### Features

- Tree-sitter parsing
- Symbol graph
- Import graph
- Call graph
- Services
- API routes
- Architecture nodes

### Deliverables

Repository

↓

Engineering Graph

↓

Queryable API

### Definition of Done

- Repository indexes automatically
- Graph updates after edits
- Nodes resolve correctly
- Architecture can be explored

---

# Milestone 3 — IDE Extension

**Goal:** Make Atlas disappear into the developer workflow.

### Features

- VS Code extension
- Cursor compatibility
- Workspace connection
- Presence updates
- Current file detection
- Current symbol detection
- Notifications

### Deliverables

Developer opens project

↓

Atlas connects automatically

↓

Developer appears online

### Definition of Done

- No CLI required
- Auto-connect works
- Live presence visible
- Notifications delivered

---

# Milestone 4 — Dashboard

**Goal:** Visualize the engineering organization.

### Screens

- Workspace
- Engineering Graph
- Workers
- Goals
- Timeline
- Reviews

### Features

- Live graph
- Animated updates
- Worker presence
- Goal progress
- Notifications
- Search

### Definition of Done

A new developer understands the project within ten seconds.

---

# Milestone 5 — Goals

**Goal:** Replace tickets with engineering goals.

### Features

- Create goal
- Update goal
- Complete goal
- Progress tracking
- Goal hierarchy
- Goal ownership

### Deliverables

Goal

↓

Tasks

↓

Reviews

↓

Complete

### Definition of Done

Every piece of engineering work belongs to a goal.

---

# Milestone 6 — Live Editing

This milestone proves Atlas's core value.

### Features

- Detect edits
- Detect active symbols
- Broadcast changes
- Live dependency notifications
- API change detection

Example

Alice edits:

PaymentService

↓

Bob immediately sees:

PaymentService

Being Modified

↓

Checkout highlighted

↓

Bob's agent receives updated context

### Definition of Done

Developers become aware of important changes before commits exist.

---

# Milestone 7 — Reviews

### Features

- Submit review
- Review queue
- Approve
- Reject
- Status tracking

### Workflow

Working

↓

Ready for Review

↓

Approved

↓

Released

### Definition of Done

Work does not complete until reviewed.

---

# Milestone 8 — AI Context API

Atlas now begins helping coding assistants.

### Features

Agent API

Engineering Graph API

Nearby context

Recent changes

Goal context

Worker context

Timeline context

Architecture context

Example

```
GET /context

returns

Goal

Nearby Workers

Recent API Changes

Current Reviews

Relevant Symbols

Architecture

Timeline
```

### Definition of Done

AI assistants receive engineering state before reading repository files.

---

# Milestone 9 — Claude / Cursor Adapters

### Goal

Integrate with existing AI coding assistants.

Adapters should:

- connect automatically
- publish presence
- publish edits
- receive notifications
- request engineering context

Atlas never generates code.

Claude continues generating code.

Atlas coordinates.

---

# Milestone 10 — Publish MVP

Requirements

Two developers

↓

Different IDEs

↓

Different AI assistants

↓

Same repository

↓

Shared engineering state

↓

Dashboard updates

↓

No manual coordination

Record a five-minute demo showing:

- Live presence
- Live editing
- Engineering graph
- Goal tracking
- Review flow
- Notifications
- AI context

If this demo works, Atlas is ready for its first users.

---

# Features Explicitly Deferred

The MVP intentionally excludes:

- Multi-repository workspaces
- Cloud hosting
- Enterprise authentication
- Permissions
- Distributed runtimes
- Mobile applications
- AI-generated task planning
- Automatic code generation
- Merge conflict resolution
- Deployment pipelines

These are future milestones.

The MVP exists to validate one idea:

> **Shared engineering state improves collaboration between humans and AI coding assistants.**

---

# Engineering Principles

Every feature added to Atlas should satisfy three questions:

1. Does this improve shared awareness?

2. Does this reduce coordination overhead?

3. Can this work across any editor or AI assistant?

If the answer is "no," reconsider adding the feature.

---

# MVP Exit Criteria

Atlas is ready for public release when the following scenario works reliably:

- Alice opens Cursor with Claude Code.
- Bob opens VS Code with another coding assistant.
- Both connect automatically.
- The Engineering Graph appears.
- Alice starts implementing a feature.
- Bob immediately sees her work.
- Bob's assistant understands the pending changes.
- Reviews happen inside Atlas.
- The feature ships with almost no coordination outside the platform.

When that workflow feels natural, Atlas has achieved its MVP.

