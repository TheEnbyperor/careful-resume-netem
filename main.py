import datetime
import json
import secrets
import typing
import uuid

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
SERVER_CONTAINER = "theenbyperor/careful-resume-netem-server:latest"
CLIENT_CONTAINER = "theenbyperor/careful-resume-netem-client:latest"


@dataclasses.dataclass
class TestParameters:
    network_delay_rtt_ms: int
    network_delay_volatility_ms: int
    network_rate_mbits: int
    packet_loss_percentage: float
    file: str


@dataclasses.dataclass
class TestResults:
    qlog_path: pathlib.Path
    data_moved: int
    data_rate: float


class TestRunner:
    def __init__(self, data_dir: pathlib.Path, test_id: str):
        self.client = docker.DockerClient(base_url=DOCKER_URL)
        self.server_container = None
        self.client_container = None
        self.network = None
        self.output_dir = data_dir / "out" / test_id
        self.bdp_dir = data_dir / "bdp" / test_id
        self.qlog_temp_dir = data_dir / "qlog-temp"
        self.bdp_key = secrets.token_bytes(32)

        os.makedirs(self.qlog_temp_dir, exist_ok=True)

    @staticmethod
    def _output_logs(name, container):
        for line in container.logs(stream=True, follow=True):
            print(f"{name}: {line.decode().strip()}", flush=True)

    def output_logs(self, name, container):
        thread = threading.Thread(target=self._output_logs, args=(name, container), daemon=True)
        thread.start()

    def create_containers(self):
        print("Creating containers", flush=True)

        network_name = str(uuid.uuid4())
        self.network = self.client.networks.create(network_name, driver="bridge", internal=True)

        container_env = {
            "RUST_LOG": "info",
            "BDP_KEY": self.bdp_key.hex(),
        }

        self.server_container = self.client.containers.run(
            image=SERVER_CONTAINER,
            detach=True,
            auto_remove=True,
            cap_add=["NET_ADMIN"],
            environment=container_env,
            network=self.network.id,
            stdout=True,
            stderr=True,
            volumes=[
                f"{self.qlog_temp_dir}:/qlog"
            ]
        )
        self.output_logs("server", self.server_container)

        os.makedirs(self.bdp_dir, exist_ok=True)
        self.client_container = self.client.containers.run(
            image=CLIENT_CONTAINER,
            detach=True,
            auto_remove=True,
            cap_add=["NET_ADMIN"],
            environment=container_env,
            network=self.network.id,
            stdout=True,
            stderr=True,
            volumes=[
                f"{self.bdp_dir}:/data"
            ]
        )
        self.output_logs("client", self.client_container)

    def destroy_containers(self):
        print("Destroying containers", flush=True)

        try:
            self.server_container.kill()
        except docker.errors.NotFound:
            pass

        try:
            self.client_container.kill()
        except docker.errors.NotFound:
            pass

        self.network.remove()

    def setup_netem(self, params: TestParameters):
        print("Setting up netem", flush=True)

        netem_limit = int(
            ((params.network_rate_mbits * 1_000_000 / 8) / 1350) * (params.network_delay_rtt_ms / 1000) * 1.5
        )

        one_way_network_delay = f"{params.network_delay_rtt_ms // 2}ms"
        volatility = f"{params.network_delay_volatility_ms // 2}ms"
        network_rate = f"{params.network_rate_mbits}mbit"
        packet_loss = f"{params.packet_loss_percentage / 2}%"

        self.server_container.exec_run([
            "/usr/sbin/tc", "qdisc", "add", "dev", "eth0", "root", "netem",
            "delay", one_way_network_delay, volatility, "rate", network_rate,
            "loss", packet_loss, "limit", str(netem_limit)
        ])
        self.client_container.exec_run([
            "/usr/sbin/tc", "qdisc", "add", "dev", "eth0", "root", "netem",
            "delay", one_way_network_delay, volatility, "rate", network_rate,
            "loss", packet_loss, "limit", str(netem_limit)
        ])

    def tear_down_netem(self):
        print("Tearing down netem", flush=True)

        self.server_container.exec_run([
            "/usr/sbin/tc", "qdisc", "del", "dev", "eth0", "root", "netem"
        ])
        self.client_container.exec_run([
            "/usr/sbin/tc", "qdisc", "del", "dev", "eth0", "root", "netem"
        ])

    def run_test(self, run: int, params: TestParameters) -> typing.Optional[TestResults]:
        print("Running test", flush=True)

        server_ip = self.client.api.inspect_container(self.server_container.id) \
            ["NetworkSettings"]["Networks"][self.network.name]["IPAddress"]

        _, output = self.client_container.exec_run(
            cmd=["/client/target/release/client"],
            environment={
                "RUST_LOG": "info",
                "SERVER_URL": f"https://{server_ip}/{params.file}",
            },
            stream=True
        )

        data = None
        for line in output:
            if line[0] == 0x1e:
                data = json.loads(line[1:].decode().strip())
            else:
                print(f"client: {line.decode().strip()}", flush=True)

        data_out_dir = self.output_dir / f"run_{run}"
        qlog_path = data_out_dir / f"connection.qlog"
        os.makedirs(data_out_dir, exist_ok=True)
        if data:
            os.rename(self.qlog_temp_dir / f"connection-{data['dcid']}.qlog", qlog_path)

        for file in os.listdir(self.qlog_temp_dir):
            os.remove(os.path.join(self.qlog_temp_dir, file))

        if data:
            with open(data_out_dir / "data.json", "w") as f:
                json.dump({
                    "params": dataclasses.asdict(params),
                    "results": data
                }, f, indent=4)

            return TestResults(
                qlog_path=qlog_path,
                data_moved=data["total"],
                data_rate=data["rate"]
            )
        else:
            return None


def main():
    with open("./tests.yaml", "r") as f:
        tests = yaml.safe_load(f)

    plots = [plot.Plots(p) for p in tests["graph_plots"]]

    script_dir = pathlib.Path(os.path.dirname(os.path.abspath(__file__)))
    batch_name = datetime.datetime.now().isoformat().replace(":", "_")
    data_dir = script_dir / "data" / batch_name

    env = latex.jinja2.make_env(loader=jinja2.loaders.FileSystemLoader(script_dir / "templates"))
    report_tpl = env.get_template('report.tex')
    report_tests = []

    for test in tests["tests"]:
        test_id = str(test["id"])
        runner = TestRunner(data_dir, test_id)
        runner.create_containers()

        report_runs = []
        for run_num, run in enumerate(test["runs"]):
            params = TestParameters(
                network_delay_rtt_ms=int(run["network_delay_rtt_ms"]),
                network_delay_volatility_ms=int(run["network_delay_volatility_ms"]),
                network_rate_mbits=int(run["network_rate_mbit"]),
                packet_loss_percentage=float(run["packet_loss_percentage"]),
                file=test["file"],
            )
            runner.setup_netem(params)
            results = runner.run_test(run_num, params)

            if not results:
                print("Test failed", flush=True)
            else:
                print(results)
                plot.plot(results.qlog_path, plots)

                report_runs.append({
                    "network_delay_rtt_ms": run["network_delay_rtt_ms"],
                    "network_delay_volatility_ms": run["network_delay_volatility_ms"],
                    "packet_loss_percentage": run["packet_loss_percentage"],
                    "network_rate_mbit": run["network_rate_mbit"],
                    "data_moved": f"{results.data_moved:,}",
                    "throughput": f"{results.data_rate:,.3f}",
                    "plot": results.qlog_path.with_suffix(".pdf"),
                })

            runner.tear_down_netem()

        report_tests.append({
            "test_file": test["file"],
            "id": test_id,
            "runs": report_runs,
        })

        runner.destroy_containers()

    tex = report_tpl.render(tests=report_tests)
    builder = latex.build.PdfLatexBuilder(pdflatex='xelatex', max_runs=2)
    pdf = builder.build_pdf(tex, texinputs=[])
    pdf.save_to(data_dir / "report.pdf")


if __name__ == "__main__":
    main()
