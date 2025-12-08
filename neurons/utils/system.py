import os
import sys
import shutil
import functools
import multiprocessing
from bittensor import logging
from constants import TEMP_FOLDER


def restart_app():
    """
    Restart the application to apply the updated changes
    """
    logging.success("App restarting due to auto-update")
    python = sys.executable
    # trunk-ignore(bandit/B606)
    os.execl(python, python, *sys.argv)


def clean_temp_files():
    """
    Clean temporary files
    """
    logging.info("Deleting temp folder...")
    folder_path = TEMP_FOLDER
    if os.path.exists(folder_path):
        logging.debug("Removing temp folder...")
        shutil.rmtree(folder_path)
    else:
        logging.info("Temp folder does not exist")


def timeout_with_multiprocess_retry(seconds, retries=3):
    """Executes a function with timeout and automatic retries using multiprocessing.

    Args:
        seconds (int): Maximum execution time in seconds before timeout
        retries (int, optional): Number of retry attempts. Defaults to 3.

    Returns:
        Decorator that wraps function with timeout and retry logic
    """

    def decorator(func):
        @functools.wraps(func)
        def wrapper(*args, **kwargs):
            for attempt in range(retries):
                logging.info(f"Attempt {attempt + 1} of {retries}")

                manager = multiprocessing.Manager()
                result_dict = manager.dict()
                process = multiprocessing.Process(
                    target=lambda d: d.update({"result": func(*args, **kwargs)}),
                    args=(result_dict,),
                )

                try:
                    process.start()
                    process.join(seconds)

                    if process.is_alive():
                        process.terminate()
                        process.join()
                        logging.warning(
                            f"Function '{func.__name__}' timed out after {seconds} seconds"
                        )
                        if attempt < retries - 1:
                            continue
                        return None

                    result = result_dict.get("result")
                    if result:
                        return result

                    if attempt < retries - 1:
                        continue

                    error_msg = (
                        "Another attempt will be made after the next request cycle."
                        if func.__name__ == "update_weights"
                        else f"Function returned {result}"
                    )
                    logging.error(f"Failed after {retries} attempts. {error_msg}")
                    return None

                finally:
                    if process.is_alive():
                        process.terminate()
                    manager.shutdown()

            return None

        return wrapper

    return decorator


def get_temp_folder() -> str:
    if not os.path.exists(TEMP_FOLDER):
        os.makedirs(TEMP_FOLDER, exist_ok=True)
    return TEMP_FOLDER
