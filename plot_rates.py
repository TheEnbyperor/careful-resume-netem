import sys
import matplotlib.pyplot as plt
import numpy as np
import pathlib
import os
import json

COLOURS = ['red', 'blue']
BAR_WIDTH = 0.4


def main():
    data_dir = pathlib.Path(sys.argv[1])
    out_dir = data_dir / "out"

    runs = {}

    for file in os.listdir(out_dir):
        for run in os.listdir(out_dir / file):
            if not run.startswith("run_"):
                continue
            run_number = int(run.split("_")[1])

            with open(out_dir / file / run / "data.json", "r") as f:
                run_data = json.load(f)

            if run_number not in runs:
                runs[run_number] = []

            runs[run_number].append((
                run_data["params"]["network_delay_rtt_ms"], run_data["results"]["rate"]
            ))

    runs = list(runs.items())
    runs.sort(key=lambda x: x[0])

    x_labels = list(set(y[0] for x in runs for y in x[1]))
    x_labels.sort()
    x_labels = [str(x) for x in x_labels]

    x_axis = np.arange(len(x_labels))

    plt.xticks(x_axis, x_labels)

    for run_number, data in runs:
        data.sort(key=lambda x: x[0])
        plt.bar(
            (x_axis + (run_number * BAR_WIDTH)) - BAR_WIDTH / 2, [x[1] for x in data],
            label=f"Run {run_number + 1}", width=BAR_WIDTH, edgecolor='white', color=COLOURS[run_number]
        )

    plt.xlabel("RTT (ms)")
    plt.ylabel("Transfer rate (Mbps)")

    # plt.xscale("log")

    plt.legend()
    plt.show()


if __name__ == "__main__":
    main()
