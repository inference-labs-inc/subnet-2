import sys
import numpy as np
import onnxruntime as ort
import traceback


def run_inference(model_path: str, input_path: str, output_path: str) -> None:
    try:
        session = ort.InferenceSession(model_path)
        input_name = session.get_inputs()[0].name
        input_data = np.load(input_path)

        options = ort.RunOptions()
        options.log_severity_level = 3

        output_names = [output.name for output in session.get_outputs()]
        outputs = session.run(output_names, {input_name: input_data}, options)

        flattened = []
        for out in outputs:
            flattened.extend(out.flatten())
        final_output = np.array(flattened)
        np.save(output_path, final_output)
    except Exception as e:
        print(f"Error running inference: {str(e)}")
        print(f"Traceback:\n{traceback.format_exc()}")
        sys.exit(1)


if __name__ == "__main__":
    if len(sys.argv) != 4:
        print("Usage: python onnx_runner.py <model_path> <input_path> <output_path>")
        sys.exit(1)
    run_inference(sys.argv[1], sys.argv[2], sys.argv[3])
