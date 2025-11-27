Model: Net (DSperse, 5-slice)

Overview
This deployment ships pre-sliced, pre-compiled DSperse slice artifacts (.dslice) for the Net model. Miners and validators do not slice/compile at runtime; they only run, prove, and verify per-slice using the DsperseHandler in library mode.

Contents
- metadata.json: declares proof_system=dsperse and points to slices/
- slices/: five prebuilt .dslice files (slice_0.dslice … slice_4.dslice)
- input.py: generates a sample input compatible with the runner

Expected run directory structure (created by Runner):
run_{timestamp}/
  slice_#/
    input.json
    output.json
    proof.json
  metadata.json
  run_results.json

How to use with DsperseHandler
The handler expects explicit DSperse settings on the session:

session.model.settings.setdefault("dsperse", {})
session.model.settings["dsperse"]["dslice_path"] = "/abs/path/to/neurons/deployment_layer/model_net/slices/slice_0.dslice"
session.model.settings["dsperse"]["run_root"] = "/abs/path/to/neurons/deployment_layer/model_net/run"

- dslice_path: absolute path to a single .dslice you want to process for this job
- run_root: directory under which timestamped run_ folders will be created

Typical flow (per slice)
1) Generate the input file for the session (JSON with key input_data):
   handler.gen_input_file(session)

2) Generate witness by running the slice (writes run_*/ with inputs/outputs and run_results.json):
   handler.generate_witness(session)

3) Generate the proof (writes proof.json under the slice subfolder of the latest run):
   proof_json_str, instances_json_str = handler.gen_proof(session)

4) Verify the proof using the latest run + dslice artifacts:
   ok = handler.verify_proof(session, validator_inputs=session.inputs, proof=proof_json_str)

Notes
- The handler operates on one slice at a time. An orchestration layer should iterate over slices and coordinate multi-miner execution if desired.
- The dslice must include the compiled EZKL artifacts (settings.json, model.compiled, vk.key, pk.key) under its ezkl/ folder.
- The included input.py generator is meant for local testing and producing a compatible input.json.
