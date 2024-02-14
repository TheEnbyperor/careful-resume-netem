import json
import typing
import yaml
import threading
import docker
import docker.errors
import pathlib
import os.path
import dataclasses
import plot
import latex.jinja2
import jinja2.loaders

DOCKER_URL = "unix:///Users/q/.docker/run/docker.sock"
NETWORK_NAME = "careful-resume-netem"
SERVER_CONTAINER = "theenbyperor/careful-resume-netem-server:latest"
CLIENT_CONTAINER = "theenbyperor/careful-resume-netem-client:latest"


@dataclasses.dataclass
class TestParameters:
    network_delay_one_way: str
    network_rate: str
    file: str
    cr_rtt_ms: typing.Optional[str]
    cr_cwnd: typing.Optional[str]


@dataclasses.dataclass
class TestResults:
    qlog_path: pathlib.Path
    data_moved: int
    data_rate: float


def _output_logs(name, container):
    for line in container.logs(stream=True, follow=True):
        print(f"{name}: {line.decode().strip()}", flush=True)


def output_logs(name, container):
    thread = threading.Thread(target=_output_logs, args=(name, container), daemon=True)
    thread.start()


def run_test(script_dir: pathlib.Path, params: TestParameters) -> typing.Optional[TestResults]:
    client = docker.DockerClient(base_url=DOCKER_URL)

    client.networks.prune()

    qlog_dir = script_dir / "qlog"
    qlog_temp_dir = script_dir / "qlog-temp"

    network = client.networks.create(NETWORK_NAME, driver="bridge", internal=True)

    print("Creating containers", flush=True)

    server_env = {
        "RUST_LOG": "info",
    }
    if params.cr_rtt_ms:
        server_env["CR_RTT_MS"] = params.cr_rtt_ms
    if params.cr_cwnd:
        server_env["CR_CWND"] = params.cr_cwnd

    server_container = client.containers.run(
        image=SERVER_CONTAINER,
        detach=True,
        auto_remove=True,
        cap_add=["NET_ADMIN"],
        environment=server_env,
        network=network.id,
        stdout=True,
        stderr=True,
        volumes=[
            f"{qlog_temp_dir}:/qlog"
        ],
        name="cr-server"
    )
    output_logs("server", server_container)

    client_container = client.containers.run(
        image=CLIENT_CONTAINER,
        detach=True,
        auto_remove=True,
        cap_add=["NET_ADMIN"],
        network=network.id,
        stdout=True,
        stderr=True,
        name="cr-client"
    )
    output_logs("client", client_container)

    print("Setting up netem", flush=True)

    server_container.exec_run([
        "/usr/sbin/tc", "qdisc", "add", "dev", "eth0", "root", "netem",
        "delay", params.network_delay_one_way, "rate", params.network_rate
    ])
    client_container.exec_run([
        "/usr/sbin/tc", "qdisc", "add", "dev", "eth0", "root", "netem",
        "delay", params.network_delay_one_way, "rate", params.network_rate
    ])

    print("Running client", flush=True)

    _, output = client_container.exec_run(
        cmd=["/client/target/release/client"],
        environment={
            "RUST_LOG": "info",
            "SERVER_URL": f"https://cr-server/{params.file}",
        },
        stream=True
    )

    data = None
    try:
        for line in output:
            if line[0] == 0x1e:
                data = json.loads(line[1:].decode().strip())
            else:
                print(f"client: {line.decode().strip()}", flush=True)
    except KeyboardInterrupt:
        server_container.kill()

    try:
        server_container.wait()
    except docker.errors.NotFound:
        pass
    client_container.kill()
    network.remove()

    if data:
        os.rename(qlog_temp_dir / f"connection-{data['dcid']}.qlog", qlog_dir / f"connection-{data['dcid']}.qlog")

    for file in os.listdir(qlog_temp_dir):
        os.remove(os.path.join(qlog_temp_dir, file))

    if data:
        return TestResults(
            qlog_path=qlog_dir / f"connection-{data['dcid']}.qlog",
            data_moved=data["total"],
            data_rate=data["rate"]
        )
    else:
        return None


def main():
    with open("./tests.yaml", "r") as f:
        tests = yaml.safe_load(f)

    script_dir = pathlib.Path(os.path.dirname(os.path.abspath(__file__)))

    env = latex.jinja2.make_env(loader=jinja2.loaders.FileSystemLoader(script_dir / "templates"))
    report_tpl = env.get_template('report.tex')

    for file in os.listdir(script_dir / "qlog"):
        os.remove(os.path.join(script_dir / "qlog", file))

    report_tests = []

    for test in tests["tests"]:
        params = TestParameters(
            network_delay_one_way=f'{test["network_delay_rtt_ms"] / 2}ms',
            network_rate=f'{test["network_rate_mbit"]}mbit',
            file=test["file"],
            cr_rtt_ms=str(test["careful_resume"]["rtt_ms"]) if "careful_resume" in test else None,
            cr_cwnd=str(test["careful_resume"]["cwnd"]) if "careful_resume" in test else None,
        )
        results = run_test(script_dir, params)
        if not results:
            print("Test failed", flush=True)
            continue

        plot.plot(results.qlog_path)
        report_tests.append({
            "network_delay_rtt_ms": test["network_delay_rtt_ms"],
            "network_rate_mbit": test["network_rate_mbit"],
            "test_file": test["file"],
            "careful_resume": test["careful_resume"] if "careful_resume" in test else None,
            "data_moved": f"{results.data_moved:,}",
            "throughput": f"{results.data_rate:,.3f}",
            "plot": results.qlog_path.with_suffix(".pdf"),
        })

    tex = report_tpl.render(tests=report_tests)
    builder = latex.build.PdfLatexBuilder(pdflatex='xelatex', max_runs=2)
    pdf = builder.build_pdf(tex, texinputs=[])
    pdf.save_to(f"report.pdf")


if __name__ == "__main__":
    main()
