import pathlib
import json
import matplotlib.pyplot as plt
import matplotlib.lines
import matplotlib.ticker
import matplotlib.patches

PHASE_COLORS = {
    "reconnaissance": "#c724ac",
    "unvalidated": "#e35914",
    "validating": "#f0c105",
    "normal": "#30f005",
    "safe_retreat": "#f00505"
}

CONGESTION_WINDOW_COLOR = "#b230c9"
BYTES_IN_FLIGHT_COLOR = "#175e1b"
SMOOTHED_RTT_COLOR = "#c95430"
CONNECTION_DATA_LIMIT_COLOR = "#ff0000"
STREAM_DATA_LIMIT_COLOR = "#ffaaaa"
DATA_SENT_COLOR = "#aaaaff"

TRIGGER_NAME = {
    "cwnd_limited": "CWND Limited",
    "packet_loss": "Packet Loss",
    "cr_mark_acknowledged": "Mark Acknowledged",
    "rtt_not_validated": "RTT Not Validated",
    "ecn_ce": "ECN Congestion",
    "exit_recovery": "Exit Recovery",
}


def qlog_records(stream):
    buffer = ''
    while True:
        chunk = stream.read(4096)
        if not chunk:
            yield buffer
            break
        buffer += chunk
        while True:
            try:
                part, buffer = buffer.split("\x1e", 1)
            except ValueError:
                break
            else:
                if part:
                    yield part.strip()


def plot(qlog_file: pathlib.Path):
    records = []
    with open(qlog_file, "r") as f:
        parts = qlog_records(f)
        header = json.loads(next(parts))
        for part in parts:
            try:
                records.append(json.loads(part))
            except json.JSONDecodeError:
                pass

    records.sort(key=lambda x: x["time"])

    min_time = next(filter(
        lambda x: x["name"] == "transport:packet_received" and x["data"]["header"]["packet_type"] == "initial",
        records
    ))["time"]
    max_time = records[-1]["time"]

    fig, ax1 = plt.subplots(figsize=(13, 9), layout='tight')
    fig.subplots_adjust(right=0.8)

    ax1.set_title(f"Connection: {header.get('title', 'unknown')}")

    ax2 = ax1.twinx()
    ax3 = ax1.twinx()
    ax2.spines.right.set_position(("axes", 1.12))

    ax1.set_autoscale_on(False)
    ax2.set_autoscale_on(False)
    ax3.set_autoscale_on(False)

    trans = ax1.get_xaxis_transform()

    plt.legend(handles=[
        matplotlib.lines.Line2D([0], [0], color=CONGESTION_WINDOW_COLOR, lw=2, label='Congestion window'),
        matplotlib.lines.Line2D([0], [0], color=BYTES_IN_FLIGHT_COLOR, lw=1, label='Bytes in flight'),
        matplotlib.lines.Line2D([0], [0], color=SMOOTHED_RTT_COLOR, lw=1, label='Smoothed RTT'),
        matplotlib.lines.Line2D([0], [0], color=CONNECTION_DATA_LIMIT_COLOR, lw=1, label='Connection data limit'),
        matplotlib.lines.Line2D([0], [0], color=STREAM_DATA_LIMIT_COLOR, lw=1, label='Sum of stream data limits'),
        matplotlib.lines.Line2D([0], [0], color=DATA_SENT_COLOR, lw=2, label='Data sent'),
        matplotlib.patches.Patch(facecolor=PHASE_COLORS["reconnaissance"], label='Reconnaissance'),
        matplotlib.patches.Patch(facecolor=PHASE_COLORS["unvalidated"], label='Unvalidated'),
        matplotlib.patches.Patch(facecolor=PHASE_COLORS["validating"], label='Validating'),
        matplotlib.patches.Patch(facecolor=PHASE_COLORS["normal"], label="Normal"),
        matplotlib.patches.Patch(facecolor=PHASE_COLORS["safe_retreat"], label='Safe Retreat'),
    ], loc="upper left")

    parameters = next(filter(
        lambda x: x["name"] == "transport:parameters_set" and x["data"]["owner"] == "remote", records
    ))

    metric_events = list(filter(lambda x: x["name"] == "recovery:metrics_updated", records))
    careful_resume_events = list(filter(lambda x: x["name"] == "recovery:careful_resume_phase_updated", records))

    last_time = max_time - min_time
    for e in reversed(careful_resume_events):
        plt.axvspan(e["time"] - min_time, last_time, facecolor=PHASE_COLORS[e["data"]["new"]], alpha=0.3, zorder=-100)
        last_time = e["time"] - min_time

        if trigger := e["data"].get("trigger"):
            plt.axvline(e["time"] - min_time, color="black", linestyle="--", zorder=-99, lw=1)
            plt.text(e["time"] - min_time, 0.5, TRIGGER_NAME[trigger], fontsize=8, rotation=90, transform=trans)

    bytes_in_flight = [
        (x["time"] - min_time , x["data"]["bytes_in_flight"]) for x in metric_events if "bytes_in_flight" in x["data"]
    ]
    congestion_window = [
        (x["time"] - min_time, x["data"]["congestion_window"]) for x in metric_events if "congestion_window" in x["data"]
    ]
    smoothed_rtt = [
        (x["time"] - min_time, x["data"]["smoothed_rtt"]) for x in metric_events if "smoothed_rtt" in x["data"]
    ]

    ax1.stairs(
        [x[1] for x in bytes_in_flight], [x[0] for x in bytes_in_flight] + [max_time],
        color=BYTES_IN_FLIGHT_COLOR, linewidth=1
    )
    ax1.stairs(
        [x[1] for x in congestion_window], [x[0] for x in congestion_window] + [max_time],
        color=CONGESTION_WINDOW_COLOR, linewidth=2
    )
    ax2.stairs(
        [x[1] for x in smoothed_rtt], [x[0] for x in smoothed_rtt] + [max_time],
        color=SMOOTHED_RTT_COLOR, linewidth=1
    )

    max_data_events = [(parameters["time"] - min_time, parameters["data"]["initial_max_data"])] + [
        (x["time"] - min_time, f["maximum"]) for x in records if x["name"] == "transport:packet_received"
        for f in x["data"]["frames"] if f["frame_type"] == "max_data"
    ]
    ax3.stairs(
        [x[1] for x in max_data_events], [x[0] for x in max_data_events] + [max_time],
        color=CONNECTION_DATA_LIMIT_COLOR, linewidth=1
    )

    max_stream_data_points = [(parameters["time"] - min_time, parameters["data"]["initial_max_stream_data_bidi_remote"])]
    stream_max = {}
    max_stream_data_events = [
        (x["time"] - min_time, f["stream_id"], f["maximum"]) for x in records if x["name"] == "transport:packet_received"
        for f in x["data"]["frames"] if f["frame_type"] == "max_stream_data"
    ]
    for t, s, m in max_stream_data_events:
        stream_max[s] = m
        max_stream_data_points.append((t, sum(stream_max.values())))
    ax3.stairs(
        [x[1] for x in max_stream_data_points], [x[0] for x in max_stream_data_points] + [max_time],
        color=STREAM_DATA_LIMIT_COLOR, linewidth=1
    )

    data_sent = 0
    packet_sent_events = [
        (x["time"] - min_time, x["data"]["raw"]["length"]) for x in records if x["name"] == "transport:packet_sent"
    ]
    data_sent_points = []
    for t, s in packet_sent_events:
        data_sent += s
        data_sent_points.append((t, data_sent))
    ax3.stairs(
        [x[1] for x in data_sent_points], [x[0] for x in data_sent_points] + [max_time],
        color=DATA_SENT_COLOR, linewidth=2
    )

    ax1.set_ylabel('Congestion bytes')
    ax3.set_ylabel('Data bytes')
    ax2.set_ylabel('Time (ms)')
    ax1.set_xlabel('Time (ms)')

    ax1.set_xlim(0, max_time - min_time)
    ax1.set_ylim(0, max([x[1] for x in bytes_in_flight] + [x[1] for x in congestion_window]) * 1.2)
    ax2.set_ylim(0, max([x[1] for x in smoothed_rtt]) * 1.2)
    ax3.set_ylim(0, max([x[1] for x in max_data_events] + [x[1] for x in max_stream_data_points]) * 1.2)

    ax1.get_xaxis().set_major_formatter(matplotlib.ticker.FuncFormatter(lambda x, p: format(int(x), ',')))
    ax1.get_yaxis().set_major_formatter(matplotlib.ticker.FuncFormatter(lambda x, p: format(int(x), ',')))
    ax2.get_yaxis().set_major_formatter(matplotlib.ticker.FuncFormatter(lambda x, p: format(int(x), ',')))
    ax3.get_yaxis().set_major_formatter(matplotlib.ticker.FuncFormatter(lambda x, p: format(int(x), ',')))

    plt.savefig(qlog_file.with_suffix('.pdf'), format="pdf", dpi=900, bbox_inches="tight")
