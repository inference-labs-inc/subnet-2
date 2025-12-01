# PM2 Configuration

> [!NOTE]
> Setting up a PM2 configuration file is completely optional. By default, all documentation assumes running subnet 2 without a configuration file.

To simplify the process of running miners and validators, we offer a template PM2 file at `ecosystem.config.tmpl.js`. This file can be modified and copied into a `ecosystem.config.js` for convenient use when starting a miner or validator.

## 1. Copy the template file

Use the below command to copy the template file into a new file called `ecosystem.config.js`.

```console
cp ecosystem.config.tmpl.js ecosystem.config.js
```

## 2. Modify the ecosystem file with your configuration

Comments are provided within the ecosystem file which outline relevant fields which need to be updated with values unique to your configuration. We also provide a full list of valid command line arguments in the [Command Line Arguments](./command_line_arguments.md) section.

You can edit the file in any text editor of your choice.

## 3. Start your miner or validator

Once your miner or validator is configured, use the following commands to easily start them.

### Miner

```console
pm2 start ecosystem.config.js --only miner
```

### Validator

```console
pm2 start ecosystem.config.js --only validator
```
