#!/bin/bash

# Search for models in the deployment layer
MODEL_DIR="neurons/deployment_layer"
for MODEL_FOLDER in $(find "$MODEL_DIR" -maxdepth 1 -type d -name 'model_*'); do
    # See if the model has metadata attached
    METADATA_FILE="${MODEL_FOLDER}/metadata.json"

    if [ ! -f "$METADATA_FILE" ]; then
        echo "Error: Metadata file not found at $METADATA_FILE"
        continue
    fi
    # If the model has a metadata file, check external_files to determine which files are needed
    external_files=$(jq -r '.external_files | to_entries[] | "\(.key) \(.value)"' "$METADATA_FILE")

    if [ $? -ne 0 ]; then
        echo "Error: Failed to parse JSON from $METADATA_FILE"
        continue
    fi

    while IFS=' ' read -r key url; do
        # If the external file already exists then do nothing
        if [ -f "${MODEL_FOLDER}/${key}" ]; then
            echo "File ${key} already downloaded at ${MODEL_FOLDER}/${key}, skipping..."
            continue
        fi
        # If the file doesn't exist we'll pull from the URL specified
        echo "Downloading ${url} to ${MODEL_FOLDER}/${key}..."
        aria2c ${url} -d "${MODEL_FOLDER}" -o "${key}"
        # If the file doesn't download then we'll skip this file and echo the error
        if [ $? -ne 0 ]; then
            echo "Error: Failed to download ${url} to ${MODEL_FOLDER}/${key}"
            continue
        fi
    done <<< "$external_files"
done
