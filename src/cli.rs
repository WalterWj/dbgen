//! CLI driver of `dbgen`.

use crate::{
    eval::{CompileContext, State, Table},
    format::{CsvFormat, Format, SqlFormat},
    parser::{QName, Template},
    value::TIMESTAMP_FORMAT,
};

use anyhow::{bail, Context, Error};
use chrono::{NaiveDateTime, ParseResult, Utc};
use chrono_tz::Tz;
use data_encoding::{DecodeError, DecodeKind, HEXLOWER_PERMISSIVE};
use flate2::write::GzEncoder;
use muldiv::MulDiv;
use pbr::{MultiBar, Units};
use rand::{
    rngs::{mock::StepRng, OsRng, StdRng},
    Rng, RngCore, SeedableRng,
};
use rayon::{
    iter::{IntoParallelIterator, ParallelIterator},
    ThreadPoolBuilder,
};
use serde_derive::Deserialize;
use std::{
    convert::TryInto,
    error,
    fs::{create_dir_all, read_to_string, File},
    io::{self, sink, stdin, BufWriter, Read, Write},
    mem,
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread::{sleep, spawn},
    time::Duration,
};
use structopt::{
    clap::AppSettings::{NextLineHelp, UnifiedHelpMessage},
    StructOpt,
};
use xz2::write::XzEncoder;

/// Arguments to the `dbgen` CLI program.
#[derive(StructOpt, Debug, Deserialize)]
#[serde(default)]
#[structopt(long_version(crate::FULL_VERSION), settings(&[NextLineHelp, UnifiedHelpMessage]))]
pub struct Args {
    /// Keep the qualified name when writing the SQL statements.
    #[structopt(long)]
    pub qualified: bool,

    /// Override the table name.
    #[structopt(short, long, conflicts_with("schema-name"))]
    pub table_name: Option<String>,

    /// Override the schema name.
    #[structopt(long)]
    pub schema_name: Option<String>,

    /// Output directory.
    #[structopt(short, long, parse(from_os_str))]
    pub out_dir: PathBuf,

    /// Number of files to generate.
    #[structopt(short = "k", long, default_value = "1")]
    pub files_count: u32,

    /// Number of INSERT statements per file.
    #[structopt(short = "n", long, default_value = "1")]
    pub inserts_count: u32,

    /// Number of rows per INSERT statement.
    #[structopt(short, long, default_value = "1")]
    pub rows_count: u32,

    /// Number of INSERT statements in the last file.
    #[structopt(long)]
    pub last_file_inserts_count: Option<u32>,

    /// Number of rows of the last INSERT statement of the last file.
    #[structopt(long)]
    pub last_insert_rows_count: Option<u32>,

    /// Escape backslashes when writing a string.
    #[structopt(long)]
    pub escape_backslash: bool,

    /// Generation template file.
    #[structopt(short = "i", long, parse(from_os_str))]
    pub template: PathBuf,

    /// Random number generator seed (should have 64 hex digits).
    #[structopt(short, long, parse(try_from_str = seed_from_str))]
    pub seed: Option<<StdRng as SeedableRng>::Seed>,

    /// Number of jobs to run in parallel, default to number of CPUs.
    #[structopt(short, long, default_value = "0")]
    pub jobs: usize,

    /// Random number generator engine
    #[structopt(long, possible_values(&["chacha", "hc128", "isaac", "isaac64", "xorshift", "pcg32", "step"]), default_value = "hc128")]
    pub rng: RngName,

    /// Disable progress bar.
    #[structopt(short, long)]
    pub quiet: bool,

    /// Time zone used for timestamps
    #[structopt(long, default_value = "UTC")]
    pub time_zone: Tz,

    /// Override the current timestamp (always in UTC), in the format "YYYY-mm-dd HH:MM:SS.fff".
    #[structopt(long, parse(try_from_str = now_from_str))]
    pub now: Option<NaiveDateTime>,

    /// Output format
    #[structopt(short, long, possible_values(&["sql", "csv"]), default_value = "sql")]
    pub format: FormatName,

    /// Compress data output
    #[structopt(short, long, possible_values(&["gzip", "gz", "xz", "zstd", "zst"]))]
    pub compression: Option<CompressionName>,

    /// Compression level (0-9 for gzip and xz, 1-21 for zstd)
    #[structopt(long, default_value = "6")]
    pub compress_level: u8,

    /// Do not generate schema files (the CREATE TABLE *.sql files)
    #[structopt(long)]
    pub no_schemas: bool,

    /// Do not generate data files (only useful for benchmarking and fuzzing)
    #[structopt(long, hidden(true))]
    pub no_data: bool,

    /// Initializes the template with these global expressions.
    #[structopt(long, short = "D")]
    pub initialize: Vec<String>,
}

/// The default implementation of the argument suitable for *testing*.
impl Default for Args {
    fn default() -> Self {
        Self {
            qualified: false,
            table_name: None,
            schema_name: None,
            out_dir: PathBuf::default(),
            files_count: 1,
            inserts_count: 1,
            rows_count: 1,
            last_file_inserts_count: None,
            last_insert_rows_count: None,
            escape_backslash: false,
            template: PathBuf::default(),
            seed: None,
            jobs: 0,
            rng: RngName::Hc128,
            quiet: true,
            time_zone: Tz::UTC,
            now: None,
            format: FormatName::Sql,
            compression: None,
            compress_level: 6,
            no_schemas: false,
            no_data: false,
            initialize: Vec::new(),
        }
    }
}

/// Parses a 64-digit hex string into an RNG seed.
pub(crate) fn seed_from_str(s: &str) -> Result<<StdRng as SeedableRng>::Seed, DecodeError> {
    let mut seed = <StdRng as SeedableRng>::Seed::default();

    if HEXLOWER_PERMISSIVE.decode_len(s.len())? != seed.len() {
        return Err(DecodeError {
            position: s.len(),
            kind: DecodeKind::Length,
        });
    }
    match HEXLOWER_PERMISSIVE.decode_mut(s.as_bytes(), &mut seed) {
        Ok(_) => Ok(seed),
        Err(e) => Err(e.error),
    }
}

fn now_from_str(s: &str) -> ParseResult<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, TIMESTAMP_FORMAT)
}

/// Extension trait for `Result` to annotate it with a file path.
trait PathResultExt {
    type Ok;
    fn with_path(self, path: &Path) -> Result<Self::Ok, Error>;
}

impl<T, E: error::Error + Send + Sync + 'static> PathResultExt for Result<T, E> {
    type Ok = T;
    fn with_path(self, path: &Path) -> Result<T, Error> {
        self.with_context(|| format!("with file {}...", path.display()))
    }
}

/// Indicator whether all tables are written. Used by the progress bar thread to break the loop.
static WRITE_FINISHED: AtomicBool = AtomicBool::new(false);
/// Counter of number of rows being written.
static WRITE_PROGRESS: AtomicU64 = AtomicU64::new(0);
/// Counter of number of bytes being written.
static WRITTEN_SIZE: AtomicU64 = AtomicU64::new(0);

/// Reads the template file
fn read_template_file(path: &Path) -> Result<String, Error> {
    if path == Path::new("-") {
        let mut buf = String::new();
        stdin().read_to_string(&mut buf).map(move |_| buf)
    } else {
        read_to_string(path)
    }
    .context("failed to read template")
}

/// Runs the CLI program.
pub fn run(args: Args) -> Result<(), Error> {
    let input = read_template_file(&args.template)?;
    let mut template = Template::parse(&input, &args.initialize, args.schema_name.as_deref())?;

    let pool = ThreadPoolBuilder::new()
        .num_threads(args.jobs)
        .build()
        .context("failed to configure thread pool")?;

    if let Some(override_table_name) = &args.table_name {
        if template.tables.len() != 1 {
            bail!("cannot use --table-name when template contains multiple tables");
        }
        template.tables[0].name = QName::parse(override_table_name)?;
    }

    let mut ctx = CompileContext::new(template.variables_count);
    ctx.time_zone = args.time_zone;
    ctx.current_timestamp = args.now.unwrap_or_else(|| Utc::now().naive_utc());
    let tables = template
        .tables
        .into_iter()
        .map(|t| ctx.compile_table(t))
        .collect::<Result<_, _>>()?;

    create_dir_all(&args.out_dir).context("failed to create output directory")?;

    let compress_level = args.compress_level;
    let env = Env {
        out_dir: args.out_dir,
        file_num_digits: args.files_count.to_string().len(),
        tables,
        qualified: args.qualified,
        rows_count: args.rows_count,
        escape_backslash: args.escape_backslash,
        format: args.format,
        compression: args.compression.map(|c| (c, compress_level)),
        no_data: args.no_data,
    };

    if !args.no_schemas {
        env.write_schema()?;
    }

    let meta_seed = args.seed.unwrap_or_else(|| OsRng.gen());
    let show_progress = !args.quiet;
    if show_progress {
        println!("Using seed: {}", HEXLOWER_PERMISSIVE.encode(&meta_seed));
    }
    let mut seeding_rng = StdRng::from_seed(meta_seed);

    let files_count = args.files_count;
    let rows_per_file = u64::from(args.inserts_count) * u64::from(args.rows_count);
    let rng_name = args.rng;
    let mut inserts_count = args.inserts_count;
    let mut rows_count = args.rows_count;
    let last_file_inserts_count = args.last_file_inserts_count.unwrap_or(inserts_count);
    let last_insert_rows_count = args.last_insert_rows_count.unwrap_or(rows_count);

    // Evaluate the global expressions if necessary.
    if !template.global_exprs.is_empty() {
        let row_gen = ctx.compile_row(template.global_exprs)?;
        let mut state = State::new(0, rng_name.create(&mut seeding_rng), ctx);
        row_gen.eval(&mut state)?;
        ctx = state.into_compile_context();
    }

    let progress_bar_thread = spawn(move || {
        if show_progress {
            run_progress_thread(
                u64::from(files_count - 1) * rows_per_file
                    + u64::from(last_file_inserts_count - 1) * u64::from(rows_count)
                    + u64::from(last_insert_rows_count),
            );
        }
    });

    let iv = (0..files_count)
        .map(move |i| {
            let file_index = i + 1;
            if file_index == files_count {
                inserts_count = last_file_inserts_count;
                rows_count = last_insert_rows_count;
            }
            (
                rng_name.create(&mut seeding_rng),
                FileInfo {
                    file_index,
                    inserts_count,
                    last_insert_rows_count: rows_count,
                },
                u64::from(i) * rows_per_file + 1,
            )
        })
        .collect::<Vec<_>>();
    let res = pool.install(move || {
        iv.into_par_iter().try_for_each(|(seed, file_info, row_num)| {
            let mut state = State::new(row_num, seed, ctx.clone());
            env.write_data_file(&file_info, &mut state)
        })
    });

    WRITE_FINISHED.store(true, Ordering::Relaxed);
    progress_bar_thread.join().unwrap();

    res?;
    Ok(())
}

/// Names of random number generators supported by `dbgen`.
#[derive(Copy, Clone, Debug, Deserialize)]
pub enum RngName {
    /// ChaCha20
    ChaCha,
    /// HC-128
    Hc128,
    /// ISAAC
    Isaac,
    /// ISAAC-64
    Isaac64,
    /// Xorshift
    XorShift,
    /// PCG32
    Pcg32,
    /// Mock RNG which steps by a constant.
    Step,
}

impl FromStr for RngName {
    type Err = Error;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "chacha" => Self::ChaCha,
            "hc128" => Self::Hc128,
            "isaac" => Self::Isaac,
            "isaac64" => Self::Isaac64,
            "xorshift" => Self::XorShift,
            "pcg32" => Self::Pcg32,
            "step" => Self::Step,
            _ => bail!("Unsupported RNG {}", name),
        })
    }
}

impl RngName {
    /// Creates an RNG engine given the name. The RNG engine instance will be seeded from `src`.
    fn create(self, src: &mut StdRng) -> Box<dyn RngCore + Send> {
        match self {
            Self::ChaCha => Box::new(rand_chacha::ChaChaRng::from_seed(src.gen())),
            Self::Hc128 => Box::new(rand_hc::Hc128Rng::from_seed(src.gen())),
            Self::Isaac => Box::new(rand_isaac::IsaacRng::from_seed(src.gen())),
            Self::Isaac64 => Box::new(rand_isaac::Isaac64Rng::from_seed(src.gen())),
            Self::XorShift => Box::new(rand_xorshift::XorShiftRng::from_seed(src.gen())),
            Self::Pcg32 => Box::new(rand_pcg::Pcg32::from_seed(src.gen())),
            Self::Step => Box::new(StepRng::new(src.next_u64(), src.next_u64() | 1)),
        }
    }
}

/// Names of output formats supported by `dbgen`.
#[derive(Copy, Clone, Debug, Deserialize)]
pub enum FormatName {
    /// SQL
    Sql,
    /// Csv
    Csv,
}

impl FromStr for FormatName {
    type Err = Error;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "sql" => Self::Sql,
            "csv" => Self::Csv,
            _ => bail!("Unsupported output format {}", name),
        })
    }
}

impl FormatName {
    /// Obtains the file extension when using this format.
    fn extension(self) -> &'static str {
        match self {
            Self::Sql => "sql",
            Self::Csv => "csv",
        }
    }

    /// Creates a formatter writer given the name.
    fn create(self, escape_backslash: bool) -> Box<dyn Format> {
        match self {
            Self::Sql => Box::new(SqlFormat { escape_backslash }),
            Self::Csv => Box::new(CsvFormat { escape_backslash }),
        }
    }
}

/// Names of the compression output formats supported by `dbgen`.
#[derive(Copy, Clone, Debug, Deserialize)]
pub enum CompressionName {
    /// Compress as gzip format (`*.gz`).
    Gzip,
    /// Compress as xz format (`*.xz`).
    Xz,
    /// Compress as Zstandard format (`*.zst`).
    Zstd,
}

impl FromStr for CompressionName {
    type Err = Error;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "gzip" | "gz" => Self::Gzip,
            "xz" => Self::Xz,
            "zstd" | "zst" => Self::Zstd,
            _ => bail!("Unsupported compression format {}", name),
        })
    }
}

impl CompressionName {
    /// Obtains the file extension when using this format.
    fn extension(self) -> &'static str {
        match self {
            Self::Gzip => "gz",
            Self::Xz => "xz",
            Self::Zstd => "zst",
        }
    }

    /// Wraps a writer with a compression layer on top.
    fn wrap<'a, W: Write + 'a>(self, inner: W, level: u8) -> Box<dyn Write + 'a> {
        match self {
            Self::Gzip => Box::new(GzEncoder::new(inner, flate2::Compression::new(level.into()))),
            Self::Xz => Box::new(XzEncoder::new(inner, level.into())),
            Self::Zstd => Box::new(
                zstd::Encoder::new(inner, level.into())
                    .expect("valid zstd encoder")
                    .auto_finish(),
            ),
        }
    }
}

/// Wrapping of a [`Write`] which counts how many bytes are written.
struct WriteCountWrapper<W: Write> {
    inner: W,
    count: u64,
}
impl<W: Write> WriteCountWrapper<W> {
    /// Creates a new [`WriteCountWrapper`] by wrapping another [`Write`].
    fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }

    /// Commits the number of bytes written into the [`WRITTEN_SIZE`] global variable, and resets
    /// the byte count of all iterated `WriteCountWrapper` instances to zero.
    fn commit_bytes_written(instances: &mut [Self]) {
        let total = instances.iter_mut().map(|w| mem::take(&mut w.count)).sum();
        WRITTEN_SIZE.fetch_add(total, Ordering::Relaxed);
    }
}

impl<W: Write> Write for WriteCountWrapper<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bytes_written = self.inner.write(buf)?;
        self.count += bytes_written as u64;
        Ok(bytes_written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The environmental data shared by all data writers.
struct Env {
    out_dir: PathBuf,
    file_num_digits: usize,
    tables: Vec<Table>,
    qualified: bool,
    rows_count: u32,
    escape_backslash: bool,
    format: FormatName,
    compression: Option<(CompressionName, u8)>,
    no_data: bool,
}

/// Information specific to a file and its derived tables.
struct FileInfo {
    file_index: u32,
    inserts_count: u32,
    last_insert_rows_count: u32,
}

struct FileWriterEnv<'a> {
    env: &'a Env,
    state: &'a mut State,
    format: Box<dyn Format>,
    files: Vec<WriteCountWrapper<BufWriter<Box<dyn Write>>>>,
    paths: Vec<PathBuf>,
    /// For each single main row, records whether the table has been visited.
    visited: Vec<bool>,
    /// For each INSERT statement, records number of rows included.
    actual_rows: Vec<u64>,
}

impl Env {
    /// Writes the `CREATE TABLE` schema files.
    fn write_schema(&self) -> Result<(), Error> {
        for table in &self.tables {
            let path = self.out_dir.join(format!("{}-schema.sql", table.name.unique_name()));
            let mut file = BufWriter::new(File::create(&path).with_path(&path)?);
            write!(
                file,
                "CREATE TABLE {} {}",
                table.name.table_name(self.qualified),
                table.content
            )
            .with_path(&path)?;
        }
        Ok(())
    }

    fn open_data_file(&self, path: &mut PathBuf) -> Result<Box<dyn Write>, Error> {
        Ok(if self.no_data {
            Box::new(sink())
        } else if let Some((compression, level)) = self.compression {
            let mut path_string = mem::take(path).into_os_string();
            path_string.push(".");
            path_string.push(compression.extension());
            *path = PathBuf::from(path_string);
            compression.wrap(File::create(&path).with_path(&path)?, level)
        } else {
            Box::new(File::create(&path).with_path(&path)?)
        })
    }

    /// Writes the data file.
    fn write_data_file(&self, info: &FileInfo, state: &mut State) -> Result<(), Error> {
        let path_suffix = format!(
            ".{0:01$}.{2}",
            info.file_index,
            self.file_num_digits,
            self.format.extension()
        );
        let format = self.format.create(self.escape_backslash);

        let mut files = Vec::with_capacity(self.tables.len());
        let mut paths = Vec::with_capacity(self.tables.len());
        for table in &self.tables {
            let mut path = self.out_dir.join([table.name.unique_name(), &path_suffix].concat());
            let inner_writer = self.open_data_file(&mut path)?;
            files.push(WriteCountWrapper::new(BufWriter::new(inner_writer)));
            paths.push(path);
        }

        let mut fwe = FileWriterEnv {
            env: self,
            state,
            format,
            files,
            paths,
            visited: vec![false; self.tables.len()],
            actual_rows: vec![0; self.tables.len()],
        };

        // for ((file, path), table) in files.iter_mut().zip(&self.tables) {
        //     format.write_header(file, table.name.table_name(self.qualified)).with_path(path)?;
        // }

        // if row_index != 0 {
        //     format.write_row_separator(&mut file).with_path(&path)?;
        // }

        // format.write_trailer(&mut file).with_path(&path)?;

        for i in 0..info.inserts_count {
            let rows_count = if i == info.inserts_count - 1 {
                info.last_insert_rows_count
            } else {
                self.rows_count
            };
            for _ in 0..rows_count {
                fwe.write_row()?;
            }
            fwe.write_trailer()?;

            WriteCountWrapper::commit_bytes_written(&mut fwe.files);
            WRITE_PROGRESS.fetch_add(rows_count.into(), Ordering::Relaxed);
        }
        Ok(())
    }
}

impl<'e> FileWriterEnv<'e> {
    fn write_one_row(&mut self, i: usize) -> Result<(), Error> {
        let file = &mut self.files[i];
        let table = &self.env.tables[i];
        let actual_rows = &mut self.actual_rows[i];

        if self.state.sub_row_num == 1 && *actual_rows == 0 {
            self.format
                .write_header(file, table.name.table_name(self.env.qualified))
        } else {
            self.format.write_row_separator(file)
        }?;

        let values = table.row.eval(self.state)?;
        *actual_rows += 1;

        for (col_index, value) in values.iter().enumerate() {
            if col_index != 0 {
                self.format.write_value_separator(file)?;
            }
            self.format.write_value(file, value)?;
        }

        for (child, count) in &table.derived {
            let count = count.eval(self.state)?;
            let count: u64 = count.try_into().with_context(|| {
                format!(
                    "number of rows to generate for {} is not an integer",
                    self.env.tables[*child].name.table_name(true),
                )
            })?;

            self.visited[*child] = true;
            for r in 1..=count {
                self.state.sub_row_num = r;
                self.write_one_row(*child)?;
            }
        }

        Ok(())
    }

    fn write_row(&mut self) -> Result<(), Error> {
        for visited in &mut self.visited {
            *visited = false;
        }
        for i in 0..self.visited.len() {
            if !mem::replace(&mut self.visited[i], true) {
                self.state.sub_row_num = 1;
                self.write_one_row(i)?;
            }
        }
        self.state.increase_row_num();
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<(), Error> {
        for ((file, path), actual_rows) in self.files.iter_mut().zip(&self.paths).zip(&mut self.actual_rows) {
            if mem::take(actual_rows) > 0 {
                self.format.write_trailer(file).with_path(path)?;
            }
        }
        Ok(())
    }
}

/// Runs the progress bar thread.
///
/// This function will loop and update the progress bar every 0.5 seconds, until [`WRITE_FINISHED`]
/// becomes `true`.
fn run_progress_thread(total_rows: u64) {
    #[allow(clippy::non_ascii_literal)]
    const TICK_FORMAT: &str = "🕐🕑🕒🕓🕔🕕🕖🕗🕘🕙🕚🕛";

    let mut mb = MultiBar::new();

    let mut pb = mb.create_bar(total_rows);

    let mut speed_bar = mb.create_bar(0);
    speed_bar.set_units(Units::Bytes);
    speed_bar.show_percent = false;
    speed_bar.show_time_left = false;
    speed_bar.show_tick = true;
    speed_bar.show_bar = false;
    speed_bar.tick_format(TICK_FORMAT);

    pb.message("Progress ");
    speed_bar.message("Size     ");

    let mb_thread = spawn(move || mb.listen());

    while !WRITE_FINISHED.load(Ordering::Relaxed) {
        sleep(Duration::from_millis(500));
        let rows_count = WRITE_PROGRESS.load(Ordering::Relaxed);
        pb.set(rows_count);

        let written_size = WRITTEN_SIZE.load(Ordering::Relaxed);
        if rows_count != 0 {
            speed_bar.total = written_size
                .mul_div_round(total_rows, rows_count)
                .unwrap_or_else(u64::max_value);
            speed_bar.set(written_size);
        }
    }

    pb.finish_println("Done!");
    speed_bar.finish();

    mb_thread.join().unwrap();
}
