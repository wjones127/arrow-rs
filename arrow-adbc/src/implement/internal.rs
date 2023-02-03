// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.
#![doc(hidden)]

use std::{
    ffi::{c_void, CStr},
    ptr::null_mut,
    rc::Rc,
    str::Utf8Error,
    sync::Arc,
};

use arrow::{
    array::{make_array_from_raw, StringArray},
    datatypes::{DataType, Field, Schema, SchemaRef},
    error::ArrowError,
    ffi::{FFI_ArrowArray, FFI_ArrowSchema},
    ffi_stream::{export_reader_into_raw, ArrowArrayStreamReader, FFI_ArrowArrayStream},
    record_batch::{RecordBatch, RecordBatchReader},
};

use crate::{
    check_err,
    error::{AdbcStatusCode, FFI_AdbcError},
    ffi::{
        AdbcObjectDepth, FFI_AdbcConnection, FFI_AdbcDatabase, FFI_AdbcDriver,
        FFI_AdbcPartitions, FFI_AdbcStatement,
    },
    interface::{ConnectionApi, DatabaseApi, StatementApi},
};

use super::{AdbcConnectionImpl, AdbcDatabaseImpl, AdbcError, AdbcStatementImpl};

type ConnType<StatementType> = <StatementType as AdbcStatementImpl>::ConnectionType;
type DBType<StatementType> =
    <ConnType<StatementType> as AdbcConnectionImpl>::DatabaseType;

/// Initialize an ADBC driver that dispatches to the types defined by the provided
/// `StatementType`. The associated error type for the statement, connection, and
/// database types must implement [AdbcError].
pub fn init_adbc_driver<StatementType>() -> FFI_AdbcDriver
where StatementType: AdbcStatementImpl,
    <StatementType as StatementApi>::Error: AdbcError,
    <StatementType::ConnectionType as ConnectionApi>::Error: AdbcError,
    <<StatementType::ConnectionType as AdbcConnectionImpl>::DatabaseType as DatabaseApi>::Error:
        AdbcError
{
    FFI_AdbcDriver {
        private_data: null_mut(),
        private_manager: null_mut(),
        release: Some(release_adbc_driver),
        DatabaseInit: Some(database_init::<DBType<StatementType>>),
        DatabaseNew: Some(database_new::<DBType<StatementType>>),
        DatabaseRelease: Some(database_release::<DBType<StatementType>>),
        DatabaseSetOption: Some(database_set_option::<DBType<StatementType>>),
        ConnectionCommit: Some(connection_commit::<StatementType::ConnectionType>),
        ConnectionGetInfo: Some(connection_get_info::<StatementType::ConnectionType>),
        ConnectionInit: Some(connection_init::<StatementType::ConnectionType>),
        ConnectionRelease: Some(connection_release::<StatementType::ConnectionType>),
        ConnectionGetObjects: Some(connection_get_objects::<ConnType<StatementType>>),
        ConnectionGetTableSchema: Some(
            connection_get_table_schema::<ConnType<StatementType>>,
        ),
        ConnectionGetTableTypes: Some(
            connection_get_table_types::<ConnType<StatementType>>,
        ),
        ConnectionNew: Some(connection_new::<ConnType<StatementType>>),
        ConnectionReadPartition: Some(
            connection_read_partition::<ConnType<StatementType>>,
        ),
        ConnectionRollback: Some(connection_rollback::<ConnType<StatementType>>),
        ConnectionSetOption: Some(connection_set_option::<ConnType<StatementType>>),
        StatementNew: Some(statement_new::<StatementType>),
        StatementBind: Some(statement_bind::<StatementType>),
        StatementBindStream: Some(statement_bind_stream::<StatementType>),
        StatementExecutePartitions: Some(statement_execute_partitions::<StatementType>),
        StatementExecuteQuery: Some(statement_execute_query::<StatementType>),
        StatementGetParameterSchema: Some(
            statement_get_parameter_schema::<StatementType>,
        ),
        StatementPrepare: Some(statement_prepare::<StatementType>),
        StatementRelease: Some(statement_release::<StatementType>),
        StatementSetOption: Some(statement_set_option::<StatementType>),
        StatementSetSqlQuery: Some(statement_set_sql_query::<StatementType>),
        StatementSetSubstraitPlan: Some(statement_set_substrait_plan::<StatementType>),
    }
}

unsafe extern "C" fn release_adbc_driver(
    driver: *mut FFI_AdbcDriver,
    _error: *mut FFI_AdbcError,
) -> AdbcStatusCode {
    // TODO: if there is no private data is there more we should do?
    if let Some(driver) = driver.as_mut() {
        driver.release = None;
    }
    AdbcStatusCode::Ok
}

pub enum AdbcFFIError {
    InvalidState(&'static str),
    NotImplemented(&'static str),
}

impl AdbcError for AdbcFFIError {
    fn message(&self) -> &str {
        match self {
            AdbcFFIError::InvalidState(msg) => msg,
            AdbcFFIError::NotImplemented(msg) => msg,
        }
    }

    fn sqlstate(&self) -> [i8; 5] {
        match self {
            AdbcFFIError::InvalidState(_) => [5, 5, 0, 1, 9],
            AdbcFFIError::NotImplemented(_) => [5, 6, 0, 3, 8],
        }
    }

    fn status_code(&self) -> AdbcStatusCode {
        match self {
            AdbcFFIError::InvalidState(_) => AdbcStatusCode::InvalidState,
            AdbcFFIError::NotImplemented(_) => AdbcStatusCode::NotImplemented,
        }
    }
}

/// Given a way to get and set the private_data field, provides methods for
/// initializing, releasing, and getting a reference to the typed private_data.
///
/// The FFI types for Database, Connection, and Statements use a c_void pointer
/// labelled "private_data" to store the implementation struct. This trait
/// handles storing, retrieving, and dropping some struct (Inner) in that field.
///
/// # Safety
///
/// This trait will dereference arbitrary data stored in a `c_void` pointer. For
/// a given instance, the `Inner` type parameter must **always** be consistent.
unsafe trait PrivateDataWrapper {
    fn _set_private_data(&mut self, data: *mut c_void);
    fn _private_data(&self) -> *mut c_void;

    fn init_inner<Inner>(&mut self, data: Inner) -> Result<(), AdbcFFIError> {
        if self._private_data().is_null() {
            let data = Box::new(data);
            let private_data = Box::into_raw(data) as *mut c_void;
            self._set_private_data(private_data);
            Ok(())
        } else {
            Err(AdbcFFIError::InvalidState("Already initialized."))
        }
    }

    unsafe fn get_inner_ref<Inner>(&self) -> Result<&Inner, AdbcFFIError> {
        let private_data = self._private_data() as *const Inner;
        private_data
            .as_ref()
            .ok_or(AdbcFFIError::InvalidState("Uninitialized."))
    }

    unsafe fn get_inner_mut<Inner>(&self) -> Result<&mut Inner, AdbcFFIError> {
        let private_data = self._private_data() as *mut Inner;
        private_data
            .as_mut()
            .ok_or(AdbcFFIError::InvalidState("Uninitialized."))
    }

    unsafe fn release_inner<Inner>(&mut self) {
        if !self._private_data().is_null() {
            // Will drop when out of scope
            let _ = Box::from_raw(self._private_data() as *mut Inner);
            // Remove dangling pointer
            self._set_private_data(null_mut());
        }
    }
}

unsafe impl PrivateDataWrapper for FFI_AdbcDatabase {
    fn _set_private_data(&mut self, data: *mut c_void) {
        self.private_data = data
    }

    fn _private_data(&self) -> *mut c_void {
        self.private_data
    }
}

unsafe impl PrivateDataWrapper for FFI_AdbcConnection {
    fn _set_private_data(&mut self, data: *mut c_void) {
        self.private_data = data
    }

    fn _private_data(&self) -> *mut c_void {
        self.private_data
    }
}

unsafe impl PrivateDataWrapper for FFI_AdbcStatement {
    fn _set_private_data(&mut self, data: *mut c_void) {
        self.private_data = data
    }

    fn _private_data(&self) -> *mut c_void {
        self.private_data
    }
}

unsafe fn try_unwrap<'a, Wrapper: PrivateDataWrapper + 'a, OutType>(
    connection: *const Wrapper,
) -> Result<&'a OutType, AdbcFFIError> {
    connection
        .as_ref()
        .ok_or(AdbcFFIError::InvalidState("Passed a null pointer."))
        .and_then(|wrapper| wrapper.get_inner_ref::<OutType>())
}

unsafe fn try_unwrap_mut<'a, Wrapper: PrivateDataWrapper + 'a, OutType>(
    connection: *mut Wrapper,
) -> Result<&'a mut OutType, AdbcFFIError> {
    connection
        .as_ref()
        .ok_or(AdbcFFIError::InvalidState("Passed a null pointer."))
        .and_then(|wrapper| wrapper.get_inner_mut::<OutType>())
}

unsafe extern "C" fn database_new<DatabaseType: Default + AdbcDatabaseImpl>(
    database: *mut FFI_AdbcDatabase,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <DatabaseType as DatabaseApi>::Error: AdbcError,
{
    let db = Arc::new(DatabaseType::default());
    let res = database
        .as_mut()
        .ok_or(AdbcFFIError::InvalidState(
            "Passed a null pointer to DatabaseNew",
        ))
        .and_then(|wrapper| wrapper.init_inner(db));

    check_err!(res, error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn database_init<DatabaseType: Default + AdbcDatabaseImpl>(
    database: *mut FFI_AdbcDatabase,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <DatabaseType as DatabaseApi>::Error: AdbcError,
{
    let inner = database
        .as_ref()
        .ok_or(AdbcFFIError::InvalidState(
            "Passed a null pointer to DatabaseInit",
        ))
        .and_then(|wrapper| wrapper.get_inner_ref::<Arc<DatabaseType>>());
    let inner = check_err!(inner, error);

    check_err!(inner.init(), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn database_set_option<DatabaseType: Default + AdbcDatabaseImpl>(
    database: *mut FFI_AdbcDatabase,
    key: *const ::std::os::raw::c_char,
    value: *const ::std::os::raw::c_char,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <DatabaseType as DatabaseApi>::Error: AdbcError,
{
    let inner: &Arc<DatabaseType> = check_err!(try_unwrap(database), error);

    // TODO: rewrite with get_maybe_str
    let key = check_err!(CStr::from_ptr(key).to_str(), error);
    let value = check_err!(CStr::from_ptr(value).to_str(), error);

    check_err!(inner.set_option(key, value), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn database_release<DatabaseType: Default + AdbcDatabaseImpl>(
    database: *mut FFI_AdbcDatabase,
    _error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <DatabaseType as DatabaseApi>::Error: AdbcError,
{
    if let Some(wrapper) = database.as_mut() {
        wrapper.release_inner::<Arc<DatabaseType>>();
    }

    AdbcStatusCode::Ok
}

// Connection

unsafe extern "C" fn connection_new<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let conn = Rc::new(ConnectionType::default());
    let res = connection
        .as_mut()
        .ok_or(AdbcFFIError::InvalidState(
            "Passed a null pointer to ConnectionNew",
        ))
        .and_then(|wrapper| wrapper.init_inner(conn));

    check_err!(res, error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_set_option<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    key: *const ::std::os::raw::c_char,
    value: *const ::std::os::raw::c_char,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    // TODO: rewrite with get_maybe_str
    let key = check_err!(CStr::from_ptr(key).to_str(), error);
    let value = check_err!(CStr::from_ptr(value).to_str(), error);

    check_err!(inner.set_option(key, value), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_init<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    database: *const FFI_AdbcDatabase,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);
    let db: &Arc<ConnectionType::DatabaseType> = check_err!(try_unwrap(database), error);

    check_err!(inner.init(db.clone()), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_release<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    _error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    if let Some(wrapper) = connection.as_mut() {
        wrapper.release_inner::<Rc<ConnectionType>>();
    }

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_get_info<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    info_codes: *const u32,
    info_codes_length: usize,
    out: *mut FFI_ArrowArrayStream,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    let info_codes = std::slice::from_raw_parts(info_codes, info_codes_length);
    let reader = check_err!(inner.get_info(info_codes), error);
    export_reader_into_raw(reader, out);

    AdbcStatusCode::Ok
}

unsafe fn get_maybe_str<'a>(
    input: *const ::std::os::raw::c_char,
) -> Result<Option<&'a str>, Utf8Error> {
    if input.is_null() {
        Ok(None)
    } else {
        CStr::from_ptr::<'a>(input).to_str().map(Some)
    }
}

unsafe extern "C" fn connection_get_objects<
    ConnectionType: Default + AdbcConnectionImpl,
>(
    connection: *mut FFI_AdbcConnection,
    depth: AdbcObjectDepth,
    catalog: *const ::std::os::raw::c_char,
    db_schema: *const ::std::os::raw::c_char,
    table_name: *const ::std::os::raw::c_char,
    table_type: *const *const ::std::os::raw::c_char,
    column_name: *const ::std::os::raw::c_char,
    out: *mut FFI_ArrowArrayStream,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    let catalog = check_err!(get_maybe_str(catalog), error);
    let db_schema = check_err!(get_maybe_str(db_schema), error);
    let table_name = check_err!(get_maybe_str(table_name), error);
    let column_name = check_err!(get_maybe_str(column_name), error);

    // table type is null-terminated sequence of char pointers
    let mut table_type_vec: Vec<&str> = Vec::new();
    let i = 0;
    while let Some(ptr) = table_type.offset(i).as_ref() {
        if !ptr.is_null() {
            let s = check_err!(CStr::from_ptr(ptr.to_owned()).to_str(), error);
            table_type_vec.push(s);
        }
    }

    let reader = check_err!(
        inner.get_objects(
            depth,
            catalog,
            db_schema,
            table_name,
            &table_type_vec,
            column_name,
        ),
        error
    );
    export_reader_into_raw(reader, out);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_get_table_schema<
    ConnectionType: Default + AdbcConnectionImpl,
>(
    connection: *mut FFI_AdbcConnection,
    catalog: *const ::std::os::raw::c_char,
    db_schema: *const ::std::os::raw::c_char,
    table_name: *const ::std::os::raw::c_char,
    out_schema: *mut FFI_ArrowSchema,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    if out_schema.is_null() {
        FFI_AdbcError::set_message(error, "Passed nullptr to schema in GetTableSchema.");
        return AdbcStatusCode::InvalidArguments;
    }

    let catalog = check_err!(get_maybe_str(catalog), error);
    let db_schema = check_err!(get_maybe_str(db_schema), error);
    let table_name = check_err!(get_maybe_str(table_name), error);

    let table_name = if let Some(table_name) = table_name {
        table_name
    } else {
        FFI_AdbcError::set_message(
            error,
            "Passed nullptr to table_name in GetTableSchema.",
        );
        return AdbcStatusCode::InvalidArguments;
    };

    let schema = check_err!(
        inner.get_table_schema(catalog, db_schema, table_name),
        error
    );

    let schema = check_err!(FFI_ArrowSchema::try_from(schema), error);

    std::ptr::write_unaligned(out_schema, schema);

    AdbcStatusCode::Ok
}

struct BatchReader {
    batch: Option<RecordBatch>,
    schema: SchemaRef,
}

impl BatchReader {
    pub fn new(batch: RecordBatch) -> Self {
        let schema = batch.schema().clone();
        Self {
            batch: Some(batch),
            schema,
        }
    }
}

impl Iterator for BatchReader {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.batch.take().map(|b| Ok(b))
    }
}

impl RecordBatchReader for BatchReader {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

unsafe extern "C" fn connection_get_table_types<
    ConnectionType: Default + AdbcConnectionImpl,
>(
    connection: *mut FFI_AdbcConnection,
    out: *mut FFI_ArrowArrayStream,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    let table_types = check_err!(inner.get_table_types(), error);

    let str_array = StringArray::from(table_types);
    let schema = Arc::new(Schema::new(vec![Field::new(
        "table_type",
        DataType::Utf8,
        false,
    )]));
    let batch = check_err!(
        RecordBatch::try_new(schema, vec![Arc::new(str_array)]),
        error
    );

    let reader = Box::new(BatchReader::new(batch));

    export_reader_into_raw(reader, out);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_read_partition<
    ConnectionType: Default + AdbcConnectionImpl,
>(
    connection: *mut FFI_AdbcConnection,
    serialized_partition: *const u8,
    serialized_length: usize,
    out: *mut FFI_ArrowArrayStream,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    let partition = std::slice::from_raw_parts(serialized_partition, serialized_length);
    let table_types = check_err!(inner.read_partition(partition), error);
    export_reader_into_raw(table_types, out);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_commit<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    check_err!(inner.commit(), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn connection_rollback<ConnectionType: Default + AdbcConnectionImpl>(
    connection: *mut FFI_AdbcConnection,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <ConnectionType as ConnectionApi>::Error: AdbcError,
{
    let inner: &Rc<ConnectionType> = check_err!(try_unwrap(connection), error);

    check_err!(inner.rollback(), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_new<StatementType: AdbcStatementImpl>(
    connection_ptr: *mut FFI_AdbcConnection,
    statement_out: *mut FFI_AdbcStatement,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let connection: &Rc<StatementType::ConnectionType> =
        check_err!(try_unwrap(connection_ptr), error);
    let statement = StatementType::new_from_connection(connection.clone());
    let res = statement_out
        .as_mut()
        .ok_or(AdbcFFIError::InvalidState(
            "Passed a null pointer to StatementNew",
        ))
        .and_then(|wrapper| wrapper.init_inner(statement));

    check_err!(res, error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_release<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    _error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    if let Some(wrapper) = statement.as_mut() {
        wrapper.release_inner::<StatementType>();
    }

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_execute_query<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    out: *mut FFI_ArrowArrayStream,
    rows_affected: *mut i64,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    let res = check_err!(statement.execute(), error);

    // Client may pass null pointer if they don't want the count
    if !rows_affected.is_null() {
        std::ptr::write_unaligned(rows_affected, res.rows_affected);
    }

    if !out.is_null() {
        if let Some(reader) = res.result {
            export_reader_into_raw(reader, out);
        } // TODO: Should there be an else here?
    }

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_prepare<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    check_err!(statement.prepare(), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_set_sql_query<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    query: *const ::std::os::raw::c_char,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    let query = check_err!(get_maybe_str(query), error).unwrap_or("");
    check_err!(statement.set_sql_query(query), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_set_substrait_plan<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    plan: *const u8,
    length: usize,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    let plan = std::slice::from_raw_parts(plan, length);
    check_err!(statement.set_substrait_plan(plan), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_bind<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    values: *const FFI_ArrowArray,
    schema: *const FFI_ArrowSchema,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    let array = check_err!(make_array_from_raw(values, schema), error);
    check_err!(statement.bind_data(array), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_bind_stream<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    stream_ptr: *mut FFI_ArrowArrayStream,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    if stream_ptr.is_null() {
        FFI_AdbcError::set_message(error, "Passed nullptr to stream in BindStream.");
        return AdbcStatusCode::InvalidArguments;
    } else {
        let reader = ArrowArrayStreamReader::from_raw(stream_ptr);
        let reader = check_err!(reader, error);
        check_err!(statement.bind_stream(Box::new(reader)), error);
    }

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_get_parameter_schema<StatementType: AdbcStatementImpl>(
    statement: *const FFI_AdbcStatement,
    out_schema: *mut FFI_ArrowSchema,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &StatementType = check_err!(try_unwrap(statement), error);

    if out_schema.is_null() {
        FFI_AdbcError::set_message(
            error,
            "Passed nullptr to schema in GetParameterSchema.",
        );
        return AdbcStatusCode::InvalidArguments;
    }

    let schema = check_err!(statement.get_param_schema(), error);
    let schema = check_err!(FFI_ArrowSchema::try_from(schema), error);
    std::ptr::write_unaligned(out_schema, schema);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_set_option<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    key: *const ::std::os::raw::c_char,
    value: *const ::std::os::raw::c_char,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    // TODO: Should we be passing empty string? I assume either way implementations have to handle it.
    let key = check_err!(get_maybe_str(key), error).unwrap_or("");
    let value = check_err!(get_maybe_str(value), error).unwrap_or("");

    check_err!(statement.set_option(key, value), error);

    AdbcStatusCode::Ok
}

unsafe extern "C" fn statement_execute_partitions<StatementType: AdbcStatementImpl>(
    statement: *mut FFI_AdbcStatement,
    out_schema: *mut FFI_ArrowSchema,
    out_partitions: *mut FFI_AdbcPartitions,
    rows_affected: *mut i64,
    error: *mut FFI_AdbcError,
) -> AdbcStatusCode
where
    <StatementType as StatementApi>::Error: AdbcError,
{
    let statement: &mut StatementType = check_err!(try_unwrap_mut(statement), error);

    if out_schema.is_null() {
        FFI_AdbcError::set_message(
            error,
            "Passed nullptr to schema in ExecutePartitions.",
        );
        return AdbcStatusCode::InvalidArguments;
    }

    if out_partitions.is_null() {
        FFI_AdbcError::set_message(
            error,
            "Passed nullptr to partitions in ExecutePartitions.",
        );
        return AdbcStatusCode::InvalidArguments;
    }

    let res = check_err!(statement.execute_partitioned(), error);

    let schema = check_err!(FFI_ArrowSchema::try_from(res.schema), error);
    std::ptr::write_unaligned(out_schema, schema);

    // Client may pass null pointer if they don't want the count
    if !rows_affected.is_null() {
        std::ptr::write_unaligned(rows_affected, res.rows_affected);
    }

    let partitions = FFI_AdbcPartitions::from(res.partition_ids);
    std::ptr::write_unaligned(out_partitions, partitions);

    AdbcStatusCode::Ok
}
