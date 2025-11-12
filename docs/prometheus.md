# Setting Up Prometheus and Grafana for Metrics Analysis

## Important Port Configuration

> [!WARNING] > **Port Conflicts**
> By default, both prometheus server and the validator prometheus exporter use port 9090. If installing on the same machine, you must either:
>
> - Change the validator exporter port: `--prometheus-port <port>`
> - Change the prometheus server port: `--web.listen-address=:<port>`

## Installation Options

Choose one of two approaches:

1. [Manual Installation](#manual-installation) - Step-by-step setup of individual components
2. [Docker Installation](#docker-installation) - Quick setup using Docker Compose

## Manual Installation

Our application provides the ability for validators to analyze some metrics with Prometheus. Validation metrics such as validation time, request times, proof sizes, ratio of verified results, and response times are served by default on port `9090`. Follow these step-by-step instructions to set up a basic Grafana UI for Prometheus metrics exposed by the validator instance.

For enabling metrics serving add `--prometheus-monitoring` flag to the validator command line.
For changing the port of the metrics server add `--prometheus-port {port_number}` flag to the validator command line.

Take a note by default Prometheus and validator data source use the same port. So in case you want to install Prometheus to the same machine as the validator, you need to change the port of the validator metrics server (with `--prometheus-port {port_number}` flag) or for Prometheus itself (with `--web.listen-address=:{port_number}` flag).

### Step 1: Install Prometheus

1. Download the latest Prometheus release from the [official website](https://prometheus.io/download/).
2. Extract the downloaded archive.
3. Navigate to the extracted directory.

### Step 2: Configure Prometheus

1. Open the `prometheus.yml` configuration file in a text editor.
2. Add a new job under the `subnet-2-validator-metrics` section to scrape metrics from the validator instance:

```yaml
scrape_configs:
  - job_name: "subnet-2-validator-metrics"
    static_configs:
      - targets: ["localhost:9090"] # Replace with validator IP if needed
```

3. Save the `prometheus.yml` file.

### Step 3: Start Prometheus

1. In the terminal, navigate to the Prometheus directory.
2. Start Prometheus by running the following command:

```sh
./prometheus --config.file=prometheus.yml
```

3. Prometheus will start and begin scraping metrics from the validator instance on port 9090.

Your Prometheus instance is now set up to fetch metrics exposed by the validator instance. You can verify the setup by opening `http://localhost:9090/targets` in a web browser and checking that the `subnet-2-validator-metrics` job is listed and the endpoint is up.

Take a look at the [Prometheus documentation](https://prometheus.io/docs/introduction/overview/) for more information on how to use Prometheus.

### Step 4: Install Grafana

1. Download the latest Grafana release from the [official website](https://grafana.com/grafana/download).
2. Extract the downloaded archive.
3. Navigate to the extracted directory.
4. Start Grafana by running `./bin/grafana-server`.
5. Open your web browser and go to `http://localhost:3000` to access the Grafana UI.

### Step 4: Add Prometheus Data Source in Grafana

1. Open your Grafana instance in a web browser.
2. Log in with your credentials. (The default username and password are `admin`.)
3. Click on the **Connections/Data Sources** in the left sidebar.
4. Click on the **Add data source** button.
5. Select **Prometheus** from the list of available data sources.
6. In the **HTTP** section, set the URL to `http://localhost:9090`.
7. Click on the **Save & Test** button to verify the connection.

### Step 5: Create a Dashboard

1. Click on **Dashboard** in the left sidebar.
2. Click on **Create Dashboard** and **Add Visualization**.
3. Select **Prometheus** as the data source.
4. Select desired metric in `Queries` tab (e.g., `proof_sizes_bytes`) and optionally label for it.
5. (Optionally) Add one more query, play with visualization options, and customize the panel as needed.
6. Click on **Save Dashboard** to save the panel.

### Step 6: Add Panels for Other Metrics

1. Repeat the process of adding new panels for each of the following metrics:

- Validation time (`validation_time_seconds`)
- Request times (`request_time_seconds`)
- Proof sizes (`proof_sizes_bytes`)
- Ratio of verified results (`verified_results_ratio`)
- Response times (`response_time_seconds`)

Your Grafana dashboard is now set up to display Prometheus metrics exposed by the validator instance. You can further customize the dashboard by adding alerts, annotations, and more.

## Docker Installation

Create a project directory with:

```
project/
├── docker-compose.yml
├── prometheus.yml
├── grafana_data/    # Will be created automatically
└── certs/           # Will be created automatically
```

## Docker Compose Configuration

Instead of installing Prometheus and Grafana manually, you can use Docker Compose to set up both services easily. Create a `docker-compose.yml` file with the following configuration:

```yaml
services:
  prometheus:
    image: prom/prometheus
    ports:
      - 9095:9090 # publish prometheus to host machine port 9095
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml
    networks:
      - monitoring
  grafana:
    image: grafana/grafana
    volumes:
      # optional section to persist grafana data
      - ./grafana_data:/var/lib/grafana
      - ./certs:/certs
    ports:
      - 3000:3000
    environment:
      - GF_SECURITY_ADMIN_PASSWORD=admin # set admin password for grafana here
    networks:
      - monitoring
networks:
  monitoring:
    driver: bridge
```

Create a `prometheus.yml` file with the configuration as shown in the previous steps. Create a `grafana_data` and `certs` directory in the same directory as the `docker-compose.yml` file. Run the following command to start Prometheus and Grafana:

```sh
docker-compose up
```

Prometheus will be available at `http://localhost:9095` and Grafana at `http://localhost:3000`. You can access both services in your web browser and set up the data source and dashboard as described in the previous steps.

For more information on using Docker Compose with Prometheus and Grafana, refer to the [official Prometheus Docker documentation](https://prometheus.io/docs/prometheus/latest/installation/).

## Security Considerations

> [!WARNING] > **Production Deployment Warning**
> The setup described above is suitable for local development. For production deployments:
>
> - Use strong passwords for Grafana
> - Configure TLS/SSL for both Prometheus and Grafana
> - Implement proper authentication mechanisms
> - Consider network isolation using Docker networks or firewalls

## Troubleshooting

1. **Verify Metrics Collection**

   - Check Prometheus targets: `http://localhost:9090/targets`
   - Confirm metrics endpoint: `curl http://localhost:9090/metrics`

2. **Common Issues**
   - Port conflicts: Check if ports 9090/3000 are already in use
   - Connection refused: Ensure validator metrics are enabled
   - No data in Grafana: Verify Prometheus data source configuration

For additional help, consult the [Prometheus troubleshooting guide](https://prometheus.io/docs/prometheus/latest/troubleshooting/).
