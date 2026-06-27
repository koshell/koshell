# Koshell Agent Instructions

## Repository Scope

This repository is the Koshell product source workspace. Keep it focused on buildable source code, public-facing project metadata, and minimal operational instructions.

Do not add long-term architecture plans, roadmap notes, future feature specifications, commercial-edition design notes, extended validation suites, private fixtures, or evaluation assets to this repository unless the project owner explicitly asks for that change.

Basic public tests that validate source behavior may live in this repository. Keep extended validation, private fixtures, evaluation assets, and commercial-edition tests in organization workspaces when they are available. If they are unavailable, do not create substitutes for those internal assets in this repository unless the project owner explicitly asks for that fallback.

## Source Repository Guidance

- Keep README and package metadata limited to what users and source contributors need to run or inspect the product.
- Keep source changes independent from private workspace checkout state.
- Keep public tests small, source-focused, and free of private fixtures or commercial-edition assumptions.
- Keep all code, configuration, and comments in English.
