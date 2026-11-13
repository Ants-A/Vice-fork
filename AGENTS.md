# Repository guidance

- When changing the interface in `ui-src/`, run `npm run build` and include the resulting tracked assets in `vice/ui/`. The application serves those built files, so source changes alone do not update the UI. Run the relevant UI checks as well.
