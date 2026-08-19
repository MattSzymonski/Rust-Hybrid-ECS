# Hot-Reload Migration Tests

See the module docstring at the top of `test_hot_reload_migration.py`
for full documentation.

## Quick start

```powershell
python tests/test_hot_reload_migration.py
python tests/test_hot_reload_migration.py --cycles 5
```

## Module-project auto-reload test

`test_module_project_auto_reload.py` verifies that editing an optional module
the project links directly (for example `pill_spline`) reloads the project as
well, so the project's embedded copy of the module code picks up the change.

```powershell
python tests/test_module_project_auto_reload.py
```
