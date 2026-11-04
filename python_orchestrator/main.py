import os
import sys
import json
import shutil
import subprocess
from dataclasses import dataclass, field, asdict
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any, Dict, List, Optional

##

class Status(str, Enum):
    PENDING = "pending"
    IN_PROGRESS = "in_progress"
    COMPLETED = "completed"
    FAILED = "failed"


@dataclass
class StepConfig:
    bin_name: str
    executor: str = "rust"  # "rust" or "python"
    script_name: Optional[str] = None
    run_after_manifest: bool = False
    expects_result_json: bool = True
    # Set to False for Python steps that are not database-pipeline steps and
    # therefore do not accept --path-build-dir (e.g. step9_build_masq.py).
    pass_build_dir: bool = True

    @property
    def step_key(self) -> str:
        return self.bin_name

    @property
    def result_file(self) -> str:
        return f"{self.bin_name}_result.json"


@dataclass
class StepStatus:
    bin_name: str
    status: Status = Status.PENDING
    started_at: Optional[str] = None
    finished_at: Optional[str] = None
    result: Optional[Dict[str, Any]] = None

    @classmethod
    def from_dict(cls, data: dict) -> "StepStatus":
        raw_status = data.get("status", "pending")
        try:
            status_enum = Status(raw_status)
        except ValueError:
            status_enum = Status.PENDING

        return cls(
            bin_name=data.get("bin_name", data.get("name", "unknown")),
            status=status_enum,
            started_at=data.get("started_at"),
            finished_at=data.get("finished_at"),
            result=data.get("result"),
        )


@dataclass
class BuildPipeline:
    current_status: Status = Status.PENDING
    failed_at_step: Optional[str] = None
    steps: List[StepStatus] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict) -> "BuildPipeline":
        steps_data = data.get("steps", [])
        steps = [StepStatus.from_dict(s) for s in steps_data]

        raw_status = data.get("current_status", "pending")
        try:
            status_enum = Status(raw_status)
        except ValueError:
            status_enum = Status.PENDING

        return cls(
            current_status=status_enum,
            failed_at_step=data.get("failed_at_step"),
            steps=steps,
        )


@dataclass
class BuildState:
    db_id: str
    build_pipeline: BuildPipeline

    @classmethod
    def from_dict(cls, data: dict) -> "BuildState":
        return cls(
            db_id=data.get("db_id", "new_digested_db"),
            build_pipeline=BuildPipeline.from_dict(data.get("build_pipeline", {})),
        )

    def to_dict(self) -> dict:
        return asdict(
            self,
            dict_factory=lambda x: {k: v.value if isinstance(v, Enum) else v for k, v in x},
        )

    def find_step(self, step_key: str) -> Optional[StepStatus]:
        for step in self.build_pipeline.steps:
            if step.bin_name == step_key:
                return step
        return None


# ==========================================
# USER EDITABLE CONFIGURATION
# ==========================================
BUILD_ENV_FILE = "db_builder.env"
STEP8_SCRIPT_NAME = "step8_package.py"
STEP9_SCRIPT_NAME = "step9_build_masq.py"

STEPS = [
    StepConfig(bin_name="step1_uniprot_xml_to_parquet"),
    StepConfig(bin_name="step2_cleavage_protein"),
    StepConfig(bin_name="step3_sort_parquet"),
    StepConfig(bin_name="step4_populate_rocks_db"),
    StepConfig(bin_name="step5_build_taxonomy_bin"),
    StepConfig(bin_name="step6_build_dict_bin"),
    StepConfig(bin_name="step7_validate_db"),
    StepConfig(
        bin_name="step8_package_db",
        executor="python",
        script_name=STEP8_SCRIPT_NAME,
        run_after_manifest=True,
        expects_result_json=False,
    ),
    # Step 9 — build tool artefacts (massq CLI binary + massq_py Python module).
    # This step is NOT part of the normal database build pipeline and is skipped
    # by default when running the full pipeline.
    #
    # Run only this step manually:
    #   PIPELINE_STEP_IDS=9 PATH_BUILD_DIR=<build_dir> python3 main.py
    #
    # Or run the script directly (no PATH_BUILD_DIR needed):
    #   python3 python_orchestrator/step9_build_masq.py             # build both
    #   python3 python_orchestrator/step9_build_masq.py --rust-only  # massq binary only
    #   python3 python_orchestrator/step9_build_masq.py --python-only # massq_py module only
    #   python3 python_orchestrator/step9_build_masq.py --python-only --release  # release wheel
    StepConfig(
        bin_name="step9_build_masq",
        executor="python",
        script_name=STEP9_SCRIPT_NAME,
        run_after_manifest=False,
        expects_result_json=False,
        pass_build_dir=False,
    ),
]

# ENV -> manifest TOML mapping.
# Prefix-based mapping (uppercase-friendly):
# MS_<SECTION>__<KEY>=value  -> [section] key = "value"
# MS_<SECTION>__<SUBSECTION>__<KEY>=value -> [section.subsection] key = "value"
MANIFEST_PREFIX = "MS_"


def parse_env_file(env_path: Path) -> Dict[str, str]:
    env_vars: Dict[str, str] = {}
    if not env_path.exists():
        return env_vars

    with open(env_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if "=" in line:
                key, val = line.split("=", 1)
                value = val.strip()
                if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
                    value = value[1:-1]
                env_vars[key.strip()] = value
    return env_vars


def parse_manifest_prefixed_env(env_vars: Dict[str, str]) -> Dict[str, Any]:
    exports: Dict[str, Any] = {}
    for env_key, value in env_vars.items():
        if not env_key.startswith(MANIFEST_PREFIX):
            continue
        suffix = env_key[len(MANIFEST_PREFIX):]
        if not suffix:
            continue
        parts = [p.strip().lower() for p in suffix.split("__") if p.strip()]
        if len(parts) < 2:
            print(f"[Warning] Skipping invalid MF key: {env_key}")
            continue
        manifest_key = ".".join(parts)
        exports[manifest_key] = value
    return exports


def extract_manifest_contrib_from_result(result_obj: Dict[str, Any]) -> Dict[str, Any]:
    contrib = result_obj.get("_manifest")
    if not isinstance(contrib, dict):
        return {}
    return {str(k): v for k, v in contrib.items() if isinstance(k, str) and k.strip()}

def normalize_manifest_entries(entries: Dict[str, Any]) -> Dict[str, Any]:
    normalized = dict(entries)
    input_file = normalized.get("source.input_file")
    if isinstance(input_file, str) and input_file.strip():
        normalized["source.input_file"] = Path(input_file).name

    # Canonicalize source keys to clearer names while keeping backward compatibility
    # with older step/env producers.
    if "source.input_file_sha256" not in normalized and "source.input_sha256" in normalized:
        normalized["source.input_file_sha256"] = normalized.pop("source.input_sha256")
    if "source.database_name" not in normalized and "source.database" in normalized:
        normalized["source.database_name"] = normalized.pop("source.database")
    if "source.database_release" not in normalized and "source.release" in normalized:
        normalized["source.database_release"] = normalized.pop("source.release")

    # Sorting section is no longer part of final manifest output.
    for key in [k for k in normalized.keys() if k == "sorting" or k.startswith("sorting.")]:
        normalized.pop(key, None)

    # Validation metrics stay in step7 result JSON, but should not be embedded
    # into distribution manifest.toml.
    for key in [k for k in normalized.keys() if k == "validation" or k.startswith("validation.")]:
        normalized.pop(key, None)

    # Keep manifest focused on core DB metadata only.
    # Internal builder/accession-dictionary details should not be exposed here.
    for key in [k for k in normalized.keys() if k == "accession_dict" or k.startswith("accession_dict.")]:
        normalized.pop(key, None)
    for key in [k for k in normalized.keys() if k == "builder" or k.startswith("builder.")]:
        normalized.pop(key, None)

    return normalized


def _format_toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    text = str(value).replace("\\", "\\\\").replace('"', '\\"')
    return f'"{text}"'


def write_manifest_toml(path: Path, values: Dict[str, Any]):
    root: Dict[str, Any] = {}
    sections: Dict[str, Dict[str, Any]] = {}

    for key, value in sorted(values.items()):
        parts = key.split(".")
        if len(parts) == 1:
            root[parts[0]] = value
        else:
            section = ".".join(parts[:-1])
            leaf = parts[-1]
            sections.setdefault(section, {})[leaf] = value

    lines: List[str] = []
    for key, value in sorted(root.items()):
        lines.append(f"{key} = {_format_toml_value(value)}")

    for section in sorted(sections.keys()):
        if lines:
            lines.append("")
        lines.append(f"[{section}]")
        for key, value in sorted(sections[section].items()):
            lines.append(f"{key} = {_format_toml_value(value)}")

    path.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")


def step_id_to_bin_name(step_cfgs: List[StepConfig]) -> Dict[int, str]:
    # Canonical step IDs are derived from STEPS list order.
    return {idx: step.bin_name for idx, step in enumerate(step_cfgs, start=1)}


def resolve_selected_steps(step_cfgs: List[StepConfig], env_vars: Dict[str, str]) -> List[StepConfig]:
    raw = env_vars.get("PIPELINE_STEP_IDS", "").strip()
    if not raw:
        return list(step_cfgs)
    id_map = step_id_to_bin_name(step_cfgs)

    requested_ids: List[int] = []
    for token in raw.split(","):
        t = token.strip()
        if not t:
            continue
        try:
            step_id = int(t)
        except ValueError:
            print(f"[!] PIPELINE_STEP_IDS contains non-integer token: '{t}'")
            sys.exit(1)

        if step_id not in id_map:
            valid = ",".join(str(k) for k in sorted(id_map.keys()))
            print(f"[!] PIPELINE_STEP_IDS contains unknown step id: {step_id} (valid: {valid})")
            sys.exit(1)
        requested_ids.append(step_id)

    if not requested_ids:
        print("[!] PIPELINE_STEP_IDS is set but empty after parsing.")
        sys.exit(1)

    requested_bins = {id_map[step_id] for step_id in requested_ids}
    selected = [step for step in step_cfgs if step.bin_name in requested_bins]

    if not selected:
        print("[!] PIPELINE_STEP_IDS selected no known steps.")
        sys.exit(1)

    selected_ids = [
        str(step_id)
        for step_id, bin_name in id_map.items()
        if bin_name in requested_bins
    ]
    print(f"[*] PIPELINE_STEP_IDS active -> selected steps: {','.join(selected_ids)}")
    return selected


def parse_bool_env(value: Optional[str], default: bool = False) -> bool:
    if value is None:
        return default
    normalized = value.strip().lower()
    if normalized in {"1", "true", "yes", "y", "on"}:
        return True
    if normalized in {"0", "false", "no", "n", "off"}:
        return False
    return default


def run_python_worker(step_cfg: StepConfig, build_dir_path: Path, builder_env: Dict[str, str]) -> bool:
    if not step_cfg.script_name:
        print(f"[!] Python step '{step_cfg.step_key}' missing script_name.")
        return False

    script_path = Path(__file__).parent / step_cfg.script_name
    if not script_path.exists():
        print(f"[!] Python step script missing: {script_path}")
        return False

    cmd = [sys.executable, str(script_path)]
    if step_cfg.pass_build_dir:
        cmd += ["--path-build-dir", str(build_dir_path)]

    # Step8-specific arguments — only appended for the packaging step.
    if step_cfg.script_name == STEP8_SCRIPT_NAME:
        step8_env = dict(builder_env)
        for key, value in os.environ.items():
            if key.startswith("STEP8_"):
                step8_env[key] = value

        if step8_env.get("STEP8_OUTPUT_DIR"):
            cmd.extend(["--path-output-dir", step8_env["STEP8_OUTPUT_DIR"]])
        if step8_env.get("STEP8_PACKAGE_NAME"):
            cmd.extend(["--package-name", step8_env["STEP8_PACKAGE_NAME"]])
        if step8_env.get("STEP8_FORMATS"):
            cmd.extend(["--formats", step8_env["STEP8_FORMATS"]])
        if step8_env.get("STEP8_DB_SCHEMA_VERSION"):
            cmd.extend(["--db-schema-version", step8_env["STEP8_DB_SCHEMA_VERSION"]])
        if step8_env.get("STEP8_MIN_MASSQ_VERSION"):
            cmd.extend(["--min-massq-version", step8_env["STEP8_MIN_MASSQ_VERSION"]])

        if parse_bool_env(step8_env.get("STEP8_KEEP_STAGING"), False):
            cmd.append("--keep-staging")
        if parse_bool_env(step8_env.get("STEP8_DRY_RUN"), False):
            cmd.append("--dry-run")
        if parse_bool_env(step8_env.get("STEP8_FORCE"), True):
            cmd.append("--force")

    print("\n" + "=" * 50)
    print(f"Running step: {step_cfg.step_key} (Python)")
    print("=" * 50)
    result = subprocess.run(cmd, cwd=Path(__file__).parent.parent.absolute(), env=os.environ.copy())

    if result.returncode == 0:
        print(f"[*] SUCCESS: {step_cfg.step_key} finished.")
        return True

    print(f"[!] ERROR: {step_cfg.step_key} failed (exit code: {result.returncode}).")
    return False


def clear_build_dir(build_dir_path: Path):
    if not build_dir_path.exists():
        print(f"[!] PATH_BUILD_DIR does not exist: {build_dir_path}")
        sys.exit(1)

    keep_file = build_dir_path / BUILD_ENV_FILE
    for child in build_dir_path.iterdir():
        if child.resolve() == keep_file.resolve():
            continue
        if child.is_dir():
            shutil.rmtree(child)
        else:
            child.unlink()

    print(f"[*] Cleaned PATH_BUILD_DIR: {build_dir_path}")
    print(f"[*] Kept: {keep_file}")


def load_build_state(state_path: Path) -> BuildState:
    if state_path.exists():
        with open(state_path, "r", encoding="utf-8") as f:
            return BuildState.from_dict(json.load(f))
    return BuildState(db_id="new_digested_db", build_pipeline=BuildPipeline())


def save_build_state(state_path: Path, state: BuildState):
    with open(state_path, "w", encoding="utf-8") as f:
        json.dump(state.to_dict(), f, indent=4)


def run_rust_worker(step_cfg: StepConfig, build_dir_path: Path, builder_env: Dict[str, str]) -> bool:
    print(f"\n{'='*50}")
    print(f"Running step: {step_cfg.step_key} (Binary: {step_cfg.bin_name})")
    print(f"{'='*50}")

    rust_project_root = Path(__file__).parent.parent.absolute()
    env_vars = os.environ.copy()
    env_vars["PATH_BUILD_DIR"] = str(build_dir_path)
    env_vars["PATH_RESULT_JSON_FILE"] = str(build_dir_path / "logs" / step_cfg.result_file)
    env_vars["PATH_LOG_DIR"] = str(build_dir_path / "logs")
    env_vars.update(builder_env)

    bin_path = rust_project_root / "target" / "release" / step_cfg.bin_name
    cmd = [str(bin_path)]

    try:
        result = subprocess.run(cmd, cwd=rust_project_root, env=env_vars)
        if result.returncode == 0:
            print(f"[*] SUCCESS: {step_cfg.step_key} finished.")
            return True
        print(f"[!] ERROR: {step_cfg.step_key} failed (exit code: {result.returncode}).")
        return False
    except Exception as e:
        print(f"[!] CRITICAL ERROR while running {step_cfg.step_key}: {e}")
        return False


def run_step_worker(step_cfg: StepConfig, build_dir_path: Path, builder_env: Dict[str, str]) -> bool:
    if step_cfg.executor == "rust":
        return run_rust_worker(step_cfg, build_dir_path, builder_env)
    if step_cfg.executor == "python":
        return run_python_worker(step_cfg, build_dir_path, builder_env)
    print(f"[!] Unknown executor '{step_cfg.executor}' for step {step_cfg.step_key}")
    return False


def prebuild_step_binaries(step_cfgs: List[StepConfig], rust_project_root: Path):
    unique_bins = sorted({step.bin_name for step in step_cfgs if step.executor == "rust"})
    if not unique_bins:
        return
    cmd = ["cargo", "build", "--release"]
    for bin_name in unique_bins:
        cmd.extend(["--bin", bin_name])

    print("\n[*] Prebuilding step binaries (cargo build --release)...")
    result = subprocess.run(cmd, cwd=rust_project_root)
    if result.returncode != 0:
        print(f"[!] Prebuild failed (exit code: {result.returncode})")
        sys.exit(1)


def execute_step(
    step_cfg: StepConfig,
    build_state: BuildState,
    state_path: Path,
    build_dir_path: Path,
    env_vars: Dict[str, str],
) -> bool:
    rerun_completed = parse_bool_env(env_vars.get("PIPELINE_RERUN_COMPLETED"), False)
    existing_step = build_state.find_step(step_cfg.step_key)

    if existing_step and existing_step.status == Status.COMPLETED:
        if not rerun_completed:
            print(f"[*] Skipping {step_cfg.step_key}, already marked as 'completed'.")
            return True
        print(f"[*] PIPELINE_RERUN_COMPLETED=true -> re-running {step_cfg.step_key}.")

    if not existing_step:
        existing_step = StepStatus(bin_name=step_cfg.step_key)
        build_state.build_pipeline.steps.append(existing_step)

    existing_step.status = Status.IN_PROGRESS
    existing_step.started_at = datetime.now(timezone.utc).isoformat()
    save_build_state(state_path, build_state)

    success = run_step_worker(step_cfg, build_dir_path, env_vars)

    if not success:
        existing_step.status = Status.FAILED
        existing_step.finished_at = datetime.now(timezone.utc).isoformat()
        build_state.build_pipeline.current_status = Status.FAILED
        build_state.build_pipeline.failed_at_step = step_cfg.step_key
        save_build_state(state_path, build_state)
        return False

    existing_step.status = Status.COMPLETED
    existing_step.finished_at = datetime.now(timezone.utc).isoformat()
    if step_cfg.expects_result_json:
        result_json_path = build_dir_path / "logs" / step_cfg.result_file
        if result_json_path.exists():
            try:
                with open(result_json_path, "r", encoding="utf-8") as json_f:
                    existing_step.result = json.load(json_f)
            except Exception as e:
                print(f"  [Warning] Could not read worker result json: {e}")
        else:
            print(f"  [Warning] Worker did not produce result {result_json_path.name}")
    save_build_state(state_path, build_state)
    return True


def orchestrate(build_dir: str):
    build_dir_path = Path(build_dir)
    env_path = build_dir_path / BUILD_ENV_FILE
    # db_builder.env is required for database pipeline steps but not for
    # tool-build steps (e.g. step9) that run without a real build directory.
    if not env_path.exists():
        if needs_build_dir := _selected_steps_need_build_dir(
            os.environ.get("PIPELINE_STEP_IDS", "")
        ):
            print(f"[!] Missing build env file: {env_path}")
            sys.exit(1)

    env_vars = parse_env_file(env_path) if env_path.exists() else {}
    pipeline_env = dict(env_vars)
    if os.environ.get("PIPELINE_STEP_IDS") is not None:
        pipeline_env["PIPELINE_STEP_IDS"] = os.environ["PIPELINE_STEP_IDS"]
    if os.environ.get("PIPELINE_RERUN_COMPLETED") is not None:
        pipeline_env["PIPELINE_RERUN_COMPLETED"] = os.environ["PIPELINE_RERUN_COMPLETED"]
    rust_project_root = Path(__file__).parent.parent.absolute()
    selected_steps = resolve_selected_steps(STEPS, pipeline_env)

    (build_dir_path / "logs").mkdir(exist_ok=True, parents=True)
    (build_dir_path / "intermediate_data").mkdir(exist_ok=True, parents=True)

    prebuild_step_binaries(selected_steps, rust_project_root)

    state_path = build_dir_path / "logs" / "build_state.json"
    build_state = load_build_state(state_path)
    build_state.build_pipeline.current_status = Status.IN_PROGRESS
    save_build_state(state_path, build_state)

    pre_manifest_steps = [step for step in selected_steps if not step.run_after_manifest]
    for step_cfg in pre_manifest_steps:
        if not execute_step(step_cfg, build_state, state_path, build_dir_path, pipeline_env):
            sys.exit(1)

    print("\n[*] Generating final manifest.toml ...")
    manifest_entries: Dict[str, Any] = {}

    # Prefix-based env exports selected by user (MS_*).
    manifest_entries.update(parse_manifest_prefixed_env(env_vars))
    if "source.input_file" not in manifest_entries and env_vars.get("PATH_INPUT_XML_GZ_FILE"):
        manifest_entries["source.input_file"] = env_vars["PATH_INPUT_XML_GZ_FILE"]

    # Step-provided manifest contributions.
    for step_cfg in selected_steps:
        step_state = build_state.find_step(step_cfg.step_key)
        if not step_state or not step_state.result:
            continue
        manifest_entries.update(extract_manifest_contrib_from_result(step_state.result))

    manifest_entries["builder.generated_at"] = datetime.now(timezone.utc).isoformat()
    manifest_entries = normalize_manifest_entries(manifest_entries)

    manifest_path = build_dir_path / "manifest.toml"
    write_manifest_toml(manifest_path, manifest_entries)

    post_manifest_steps = [step for step in selected_steps if step.run_after_manifest]
    for step_cfg in post_manifest_steps:
        if not execute_step(step_cfg, build_state, state_path, build_dir_path, pipeline_env):
            sys.exit(1)

    build_state.build_pipeline.current_status = Status.COMPLETED
    build_state.build_pipeline.failed_at_step = None
    save_build_state(state_path, build_state)

    print("\n" + "*" * 50)
    print("[*] PIPELINE COMPLETED ALL STEPS SUCCESSFULLY!")
    print(f"[*] Manifest saved to: {manifest_path}")
    print("*" * 50)


def _selected_steps_need_build_dir(pipeline_step_ids: str) -> bool:
    """Return True if any of the selected steps requires PATH_BUILD_DIR."""
    if not pipeline_step_ids.strip():
        return True  # full pipeline always needs it
    id_map = step_id_to_bin_name(STEPS)
    for token in pipeline_step_ids.split(","):
        t = token.strip()
        if not t:
            continue
        try:
            step_id = int(t)
        except ValueError:
            return True
        cfg = next((s for s in STEPS if s.bin_name == id_map.get(step_id)), None)
        if cfg is None or cfg.pass_build_dir or cfg.expects_result_json:
            return True
    return False


if __name__ == "__main__":
    target_build_dir = os.environ.get("PATH_BUILD_DIR") or os.environ.get("BUILD_DIR")

    pipeline_step_ids_raw = os.environ.get("PIPELINE_STEP_IDS", "")
    needs_build_dir = _selected_steps_need_build_dir(pipeline_step_ids_raw)

    if not target_build_dir:
        if needs_build_dir:
            print("ERROR: PATH_BUILD_DIR env variable is not set!")
            print("Usage: PATH_BUILD_DIR=/path/to/my_db python3 main.py")
            sys.exit(1)
        # Steps that don't use a build dir (e.g. step9) can run without it.
        import tempfile
        target_build_dir = tempfile.mkdtemp(prefix="massq_build_tmp_")

    cli_args = {arg.strip().lower() for arg in sys.argv[1:]}
    if "clear" in cli_args or "--clear" in cli_args:
        clear_build_dir(Path(target_build_dir))
        sys.exit(0)

    orchestrate(target_build_dir)
