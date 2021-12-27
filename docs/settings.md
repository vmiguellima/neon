## Pageserver

Pageserver is mainly configured via a `pageserver.toml` config file.
If there's no such file during `init` phase of the server, it creates the file itself. Without 'init', the file is read.

There's a possibility to pass an arbitrary config value to the pageserver binary as an argument: such values override
the values in the config file, if any are specified for the same key and get into the final config during init phase.


### Config example

```toml
# Initial configuration file created by 'pageserver --init'

listen_pg_addr = '127.0.0.1:64000'
listen_http_addr = '127.0.0.1:9898'

checkpoint_distance = '268435456' # in bytes
checkpoint_period = '1 s'

gc_period = '100 s'
gc_horizon = '67108864'

max_file_descriptors = '100'

# initial superuser role name to use when creating a new tenant
initial_superuser_name = 'zenith_admin'

# [remote_storage]
```

The config above shows default values for all basic pageserver settings.
Pageserver uses default values for all files that are missing in the config, so it's not a hard error to leave the config blank.
Yet, it validates the config values it can (e.g. postgres install dir) and errors if the validation fails, refusing to start.

Note the `[remote_storage]` section: it's a [table](https://toml.io/en/v1.0.0#table) in TOML specification and

* either has to be placed in the config after the table-less values such as `initial_superuser_name = 'zenith_admin'`

* or can be placed anywhere if rewritten in identical form as [inline table](https://toml.io/en/v1.0.0#inline-table): `remote_storage = {foo = 2}`

### Config values

All values can be passed as an argument to the pageserver binary, using the `-c` parameter and specified as a valid TOML string. All tables should be passed in the inline form.

Example: `${PAGESERVER_BIN} -c "checkpoint_period = '100 s'" -c "remote_storage={local_path='/some/local/path/'}"`

Note that TOML distinguishes between strings and integers, the former require single or double quotes around them.

#### checkpoint_distance

`checkpoint_distance` is the amount of incoming WAL that is held in
the open layer, before it's flushed to local disk. It puts an upper
bound on how much WAL needs to be re-processed after a pageserver
crash. It is a soft limit, the pageserver can momentarily go above it,
but it will trigger a checkpoint operation to get it back below the
limit.

`checkpoint_distance` also determines how much WAL needs to be kept
durable in the safekeeper.  The safekeeper must have capacity to hold
this much WAL, with some headroom, otherwise you can get stuck in a
situation where the safekeeper is full and stops accepting new WAL,
but the pageserver is not flushing out and releasing the space in the
safekeeper because it hasn't reached checkpoint_distance yet.

`checkpoint_distance` also controls how often the WAL is uploaded to
S3.

The unit is # of bytes.

#### checkpoint_period

The pageserver checks whether `checkpoint_distance` has been reached
every `checkpoint_period` seconds. Default is 1 s, which should be
fine.

#### gc_horizon

`gz_horizon` determines how much history is retained, to allow
branching and read replicas at an older point in time. The unit is #
of bytes of WAL. Page versions older than this are garbage collected
away.

#### gc_period

Interval at which garbage collection is triggered. Default is 100 s.

#### initial_superuser_name

Name of the initial superuser role, passed to initdb when a new tenant
is initialized. It doesn't affect anything after initialization. The
default is Note: The default is 'zenith_admin', and the console
depends on that, so if you change it, bad things will happen.

#### page_cache_size

Size of the page cache, to hold materialized page versions. Unit is
number of 8 kB blocks. The default is 8192, which means 64 MB.

#### max_file_descriptors

Max number of file descriptors to hold open concurrently for accessing
layer files. This should be kept well below the process/container/OS
limit (see `ulimit -n`), as the pageserver also needs file descriptors
for other files and for sockets for incoming connections.

#### pg_distrib_dir

A directory with Postgres installation to use during pageserver activities.
Inside that dir, a `bin/postgres` binary should be present.

The default distrib dir is `./tmp_install/`.

#### workdir (-D)

A directory in the file system, where pageserver will store its files.
The default is `./.zenith/`.

This parameter has a special CLI alias (`-D`) and can not be overridden with regular `-c` way.

##### Remote storage

There's a way to automatically back up and restore some of the pageserver's data from working dir to the remote storage.
The backup system is disabled by default and can be enabled for either of the currently available storages:

###### Local FS storage

Pageserver can back up and restore some of its workdir contents to another directory.
For that, only a path to that directory needs to be specified as a parameter:

```toml
[remote_storage]
local_path = '/some/local/path/'
```

###### S3 storage

Pageserver can back up and restore some of its workdir contents to S3.
Full set of S3 credentials is needed for that as parameters.
Configuration example:

```toml
[remote_storage]
# Name of the bucket to connect to
bucket_name = 'some-sample-bucket'

# Name of the region where the bucket is located at
bucket_region = 'eu-north-1'

# Access key to connect to the bucket ("login" part of the credentials)
access_key_id = 'SOMEKEYAAAAASADSAH*#'

# Secret access key to connect to the bucket ("password" part of the credentials)
secret_access_key = 'SOMEsEcReTsd292v'
```

###### General remote storage configuration

Pagesever allows only one remote storage configured concurrently and errors if parameters from multiple different remote configurations are used.
No default values are used for the remote storage configuration parameters.

Besides, there are parameters common for all types of remote storage that can be configured, those have defaults:

```toml
[remote_storage]
# Max number of concurrent connections to open for uploading to or downloading from the remote storage.
max_concurrent_sync = 100

# Max number of errors a single task can have before it's considered failed and not attempted to run anymore.
max_sync_errors = 10
```


## safekeeper

TODO