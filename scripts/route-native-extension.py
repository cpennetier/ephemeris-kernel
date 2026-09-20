"""Keep the editable native leaf in Cargo's output root when one is selected."""

import os
from pathlib import Path
import shutil
import sys
import sysconfig
import tempfile


def route_extension(repo: Path, cargo_target_dir: str | None) -> Path | None:
    if not cargo_target_dir:
        return None  # Ordinary clones retain maturin's default editable layout.
    suffix = sysconfig.get_config_var("EXT_SUFFIX")
    if not suffix:
        raise RuntimeError("the active Python has no native extension suffix")
    package = (repo / "python" / "ephemeris").resolve()
    source = package / ("_engine" + suffix)
    output = (repo / cargo_target_dir / "python-extensions").resolve()
    if output == package or package in output.parents:
        raise RuntimeError("native output must not be inside the Python package")
    destination = output / source.name
    if destination.is_symlink() or (destination.exists() and not destination.is_file()):
        raise RuntimeError(f"native output is not a regular file: {destination}")
    if source.is_symlink():
        if destination.is_file() and source.resolve() == destination:
            return destination  # A repeated routing call is already complete.
        raise RuntimeError(f"unexpected native extension symlink: {source}")
    if not source.is_file():
        raise RuntimeError(f"missing editable extension for this Python: {source}")

    output.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=".engine-", dir=output)
    os.close(descriptor)
    staged = Path(name)
    try:
        shutil.copy2(source, staged)
        # Prepare the link first: unavailable symlinks leave both old files intact.
        with tempfile.TemporaryDirectory(prefix=".engine-link-", dir=package) as scratch:
            link = Path(scratch) / source.name
            link.symlink_to(destination)
            os.replace(staged, destination)
            os.replace(link, source)
    finally:
        staged.unlink(missing_ok=True)
    return destination


if __name__ == "__main__":
    try:
        target = route_extension(Path(__file__).resolve().parents[1], os.environ.get("CARGO_TARGET_DIR"))
        if target is not None:
            print(f"editable native extension routed to {target}")
    except (OSError, RuntimeError) as error:
        print(f"native extension routing failed: {error}", file=sys.stderr)
        raise SystemExit(1)
