#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use core::ffi::{c_char, c_void};

pub type duckdb_state = i32;
pub type duckdb_database = *mut c_void;
pub type duckdb_connection = *mut c_void;
pub type duckdb_config = *mut c_void;
pub type duckdb_prepared_statement = *mut c_void;
pub type duckdb_arrow_options = *mut c_void;
pub type duckdb_error_data = *mut c_void;
pub type duckdb_appender = *mut c_void;
pub type idx_t = u64;
pub type duckdb_data_chunk = *mut c_void;
pub type duckdb_vector = *mut c_void;
pub type duckdb_logical_type = *mut c_void;
pub type duckdb_type = u32;
pub type duckdb_scalar_function = *mut c_void;
pub type duckdb_scalar_function_set = *mut c_void;
pub type duckdb_aggregate_function = *mut c_void;
pub type duckdb_aggregate_function_set = *mut c_void;
pub type duckdb_aggregate_state = *mut c_void;
pub type duckdb_table_function = *mut c_void;
pub type duckdb_init_info = *mut c_void;
pub type duckdb_function_info = *mut c_void;
pub type duckdb_bind_info = *mut c_void;
pub type duckdb_client_context = *mut c_void;
pub type duckdb_value = *mut c_void;
pub type duckdb_expression = *mut c_void;
#[repr(C)]
pub struct duckdb_string_t {
    pub value: [u8; 16],
}

#[repr(C)]
pub struct duckdb_blob {
    pub data: *mut c_void,
    pub size: idx_t,
}

pub type duckdb_delete_callback_t = Option<unsafe extern "C" fn(*mut c_void)>;
pub type duckdb_replacement_scan_info = *mut c_void;
pub type duckdb_replacement_callback_t = Option<
    unsafe extern "C" fn(
        info: duckdb_replacement_scan_info,
        table_name: *const c_char,
        data: *mut c_void,
    ),
>;
pub type duckdb_cast_function = *mut c_void;
pub type duckdb_cast_function_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        count: idx_t,
        input: duckdb_vector,
        output: duckdb_vector,
    ) -> bool,
>;
pub type duckdb_copy_callback_t =
    Option<unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> duckdb_state>;
pub type duckdb_scalar_function_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        input: duckdb_data_chunk,
        output: duckdb_vector,
    ),
>;
pub type duckdb_scalar_function_bind_t = Option<unsafe extern "C" fn(info: duckdb_bind_info)>;
pub type duckdb_table_function_bind_t = Option<unsafe extern "C" fn(info: duckdb_bind_info)>;
pub type duckdb_table_function_init_t = Option<unsafe extern "C" fn(info: duckdb_init_info)>;
pub type duckdb_table_function_t =
    Option<unsafe extern "C" fn(info: duckdb_function_info, output: duckdb_data_chunk)>;
pub type duckdb_aggregate_state_size =
    Option<unsafe extern "C" fn(info: duckdb_function_info) -> idx_t>;
pub type duckdb_aggregate_init_t =
    Option<unsafe extern "C" fn(info: duckdb_function_info, state: duckdb_aggregate_state)>;
pub type duckdb_aggregate_destroy_t =
    Option<unsafe extern "C" fn(states: *mut duckdb_aggregate_state, count: idx_t)>;
pub type duckdb_aggregate_update_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        input: duckdb_data_chunk,
        states: *mut duckdb_aggregate_state,
    ),
>;
pub type duckdb_aggregate_combine_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        source: *mut duckdb_aggregate_state,
        target: *mut duckdb_aggregate_state,
        count: idx_t,
    ),
>;
pub type duckdb_aggregate_finalize_t = Option<
    unsafe extern "C" fn(
        info: duckdb_function_info,
        source: *mut duckdb_aggregate_state,
        result: duckdb_vector,
        count: idx_t,
        offset: idx_t,
    ),
>;
pub type duckdb_column = *mut c_void;

pub const DUCKDB_TYPE_INVALID: duckdb_type = 0;
pub const DUCKDB_TYPE_BOOLEAN: duckdb_type = 1;
pub const DUCKDB_TYPE_TINYINT: duckdb_type = 2;
pub const DUCKDB_TYPE_SMALLINT: duckdb_type = 3;
pub const DUCKDB_TYPE_INTEGER: duckdb_type = 4;
pub const DUCKDB_TYPE_BIGINT: duckdb_type = 5;
pub const DUCKDB_TYPE_UTINYINT: duckdb_type = 6;
pub const DUCKDB_TYPE_USMALLINT: duckdb_type = 7;
pub const DUCKDB_TYPE_UINTEGER: duckdb_type = 8;
pub const DUCKDB_TYPE_UBIGINT: duckdb_type = 9;
pub const DUCKDB_TYPE_FLOAT: duckdb_type = 10;
pub const DUCKDB_TYPE_DOUBLE: duckdb_type = 11;
pub const DUCKDB_TYPE_VARCHAR: duckdb_type = 17;
pub const DUCKDB_TYPE_BLOB: duckdb_type = 18;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct duckdb_result {
    pub deprecated_column_count: idx_t,
    pub deprecated_row_count: idx_t,
    pub deprecated_rows_changed: idx_t,
    pub deprecated_columns: duckdb_column,
    pub deprecated_error_message: *mut c_char,
    pub internal_data: *mut c_void,
}

extern "C" {
    pub fn duckdb_open_ext(
        path: *const c_char,
        out_database: *mut duckdb_database,
        config: *mut c_void,
        out_error: *mut *mut c_char,
    ) -> duckdb_state;
    pub fn duckdb_create_config(out_config: *mut duckdb_config) -> duckdb_state;
    pub fn duckdb_set_config(
        config: duckdb_config,
        name: *const c_char,
        option: *const c_char,
    ) -> duckdb_state;
    pub fn duckdb_destroy_config(config: *mut duckdb_config);

    pub fn duckdb_close(database: *mut duckdb_database);

    pub fn duckdb_connect(
        database: duckdb_database,
        out_connection: *mut duckdb_connection,
    ) -> duckdb_state;

    pub fn duckdb_disconnect(connection: *mut duckdb_connection);

    pub fn duckdb_interrupt(connection: duckdb_connection);

    pub fn duckdb_query(
        connection: duckdb_connection,
        query: *const c_char,
        out_result: *mut duckdb_result,
    ) -> duckdb_state;

    pub fn duckdb_destroy_result(result: *mut duckdb_result);

    pub fn duckdb_prepare(
        connection: duckdb_connection,
        query: *const c_char,
        out_prepared_statement: *mut duckdb_prepared_statement,
    ) -> duckdb_state;
    pub fn duckdb_destroy_prepare(prepared_statement: *mut duckdb_prepared_statement);
    pub fn duckdb_prepare_error(prepared_statement: duckdb_prepared_statement) -> *const c_char;
    pub fn duckdb_nparams(prepared_statement: duckdb_prepared_statement) -> idx_t;
    pub fn duckdb_clear_bindings(prepared_statement: duckdb_prepared_statement) -> duckdb_state;
    pub fn duckdb_bind_boolean(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
        val: bool,
    ) -> duckdb_state;
    pub fn duckdb_bind_int64(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
        val: i64,
    ) -> duckdb_state;
    pub fn duckdb_bind_uint64(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
        val: u64,
    ) -> duckdb_state;
    pub fn duckdb_bind_double(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
        val: f64,
    ) -> duckdb_state;
    pub fn duckdb_bind_varchar_length(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
        val: *const c_char,
        length: idx_t,
    ) -> duckdb_state;
    pub fn duckdb_bind_blob(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
        data: *const c_void,
        length: idx_t,
    ) -> duckdb_state;
    pub fn duckdb_bind_null(
        prepared_statement: duckdb_prepared_statement,
        param_idx: idx_t,
    ) -> duckdb_state;
    pub fn duckdb_execute_prepared(
        prepared_statement: duckdb_prepared_statement,
        out_result: *mut duckdb_result,
    ) -> duckdb_state;

    // Modern Arrow C Data Interface API: convert a duckdb_result + its data
    // chunks to Arrow C Data Interface structs (ArrowSchema/ArrowArray passed as
    // *mut c_void). Used to export query results as Arrow IPC stream bytes.
    pub fn duckdb_result_get_arrow_options(result: *mut duckdb_result) -> duckdb_arrow_options;
    pub fn duckdb_destroy_arrow_options(arrow_options: *mut duckdb_arrow_options);
    pub fn duckdb_to_arrow_schema(
        arrow_options: duckdb_arrow_options,
        types: *mut duckdb_logical_type,
        names: *mut *const c_char,
        column_count: idx_t,
        out_schema: *mut c_void,
    ) -> duckdb_error_data;
    pub fn duckdb_data_chunk_to_arrow(
        arrow_options: duckdb_arrow_options,
        chunk: duckdb_data_chunk,
        out_arrow_array: *mut c_void,
    ) -> duckdb_error_data;
    pub fn duckdb_fetch_chunk(result: duckdb_result) -> duckdb_data_chunk;
    pub fn duckdb_destroy_data_chunk(chunk: *mut duckdb_data_chunk);
    pub fn duckdb_error_data_message(error_data: duckdb_error_data) -> *const c_char;
    pub fn duckdb_error_data_has_error(error_data: duckdb_error_data) -> bool;
    pub fn duckdb_destroy_error_data(error_data: *mut duckdb_error_data);

    // Appender: fast bulk row insertion into an existing table.
    pub fn duckdb_appender_create(
        connection: duckdb_connection,
        schema: *const c_char,
        table: *const c_char,
        out_appender: *mut duckdb_appender,
    ) -> duckdb_state;
    pub fn duckdb_appender_error(appender: duckdb_appender) -> *const c_char;
    pub fn duckdb_appender_end_row(appender: duckdb_appender) -> duckdb_state;
    pub fn duckdb_appender_flush(appender: duckdb_appender) -> duckdb_state;
    pub fn duckdb_appender_close(appender: duckdb_appender) -> duckdb_state;
    pub fn duckdb_appender_destroy(appender: *mut duckdb_appender) -> duckdb_state;
    pub fn duckdb_append_bool(appender: duckdb_appender, value: bool) -> duckdb_state;
    pub fn duckdb_append_int64(appender: duckdb_appender, value: i64) -> duckdb_state;
    pub fn duckdb_append_uint64(appender: duckdb_appender, value: u64) -> duckdb_state;
    pub fn duckdb_append_double(appender: duckdb_appender, value: f64) -> duckdb_state;
    pub fn duckdb_append_varchar_length(
        appender: duckdb_appender,
        val: *const c_char,
        length: idx_t,
    ) -> duckdb_state;
    pub fn duckdb_append_blob(
        appender: duckdb_appender,
        data: *const c_void,
        length: idx_t,
    ) -> duckdb_state;
    pub fn duckdb_append_null(appender: duckdb_appender) -> duckdb_state;

    pub fn duckdb_result_error(result: *mut duckdb_result) -> *const c_char;

    pub fn duckdb_column_count(result: *mut duckdb_result) -> idx_t;

    pub fn duckdb_row_count(result: *mut duckdb_result) -> idx_t;

    pub fn duckdb_column_name(result: *mut duckdb_result, col: idx_t) -> *const c_char;

    pub fn duckdb_value_is_null(result: *mut duckdb_result, col: idx_t, row: idx_t) -> bool;

    pub fn duckdb_value_varchar(result: *mut duckdb_result, col: idx_t, row: idx_t) -> *mut c_char;

    pub fn duckdb_free(ptr: *mut c_void);

    pub fn duckdb_create_scalar_function() -> duckdb_scalar_function;
    pub fn duckdb_destroy_scalar_function(function: *mut duckdb_scalar_function);
    pub fn duckdb_scalar_function_set_name(function: duckdb_scalar_function, name: *const c_char);
    pub fn duckdb_scalar_function_set_varargs(
        function: duckdb_scalar_function,
        varargs_type: duckdb_logical_type,
    );
    pub fn duckdb_scalar_function_set_special_handling(function: duckdb_scalar_function);
    pub fn duckdb_scalar_function_set_volatile(function: duckdb_scalar_function);
    pub fn duckdb_scalar_function_add_parameter(
        function: duckdb_scalar_function,
        param_type: duckdb_logical_type,
    );
    pub fn duckdb_scalar_function_set_return_type(
        function: duckdb_scalar_function,
        return_type: duckdb_logical_type,
    );
    pub fn duckdb_scalar_function_set_extra_info(
        function: duckdb_scalar_function,
        extra_info: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_scalar_function_set_function(
        function: duckdb_scalar_function,
        callback: duckdb_scalar_function_t,
    );
    pub fn duckdb_scalar_function_set_bind(
        function: duckdb_scalar_function,
        bind: duckdb_scalar_function_bind_t,
    );
    pub fn duckdb_scalar_function_set_bind_data(
        info: duckdb_bind_info,
        bind_data: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_scalar_function_set_bind_data_copy(
        info: duckdb_bind_info,
        copy: duckdb_copy_callback_t,
    );
    pub fn duckdb_scalar_function_bind_set_error(info: duckdb_bind_info, message: *const c_char);
    pub fn duckdb_scalar_function_set_error(info: duckdb_function_info, message: *const c_char);
    pub fn duckdb_register_scalar_function(
        connection: duckdb_connection,
        function: duckdb_scalar_function,
    ) -> duckdb_state;
    pub fn duckdb_scalar_function_get_extra_info(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_scalar_function_bind_get_extra_info(info: duckdb_bind_info) -> *mut c_void;
    pub fn duckdb_scalar_function_get_bind_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_scalar_function_bind_get_argument_count(info: duckdb_bind_info) -> idx_t;
    pub fn duckdb_scalar_function_bind_get_argument(
        info: duckdb_bind_info,
        index: idx_t,
    ) -> duckdb_expression;

    pub fn duckdb_get_bool(val: duckdb_value) -> bool;
    pub fn duckdb_get_int64(val: duckdb_value) -> i64;
    pub fn duckdb_get_uint64(val: duckdb_value) -> u64;
    pub fn duckdb_get_double(val: duckdb_value) -> f64;
    pub fn duckdb_get_varchar(val: duckdb_value) -> *mut c_char;
    pub fn duckdb_get_blob(val: duckdb_value) -> duckdb_blob;
    pub fn duckdb_is_null_value(val: duckdb_value) -> bool;
    pub fn duckdb_destroy_value(val: *mut duckdb_value);

    pub fn duckdb_create_aggregate_function() -> duckdb_aggregate_function;
    pub fn duckdb_destroy_aggregate_function(function: *mut duckdb_aggregate_function);
    pub fn duckdb_aggregate_function_set_name(
        function: duckdb_aggregate_function,
        name: *const c_char,
    );
    pub fn duckdb_aggregate_function_add_parameter(
        function: duckdb_aggregate_function,
        param_type: duckdb_logical_type,
    );
    pub fn duckdb_aggregate_function_set_return_type(
        function: duckdb_aggregate_function,
        return_type: duckdb_logical_type,
    );
    pub fn duckdb_aggregate_function_set_functions(
        function: duckdb_aggregate_function,
        state_size: duckdb_aggregate_state_size,
        state_init: duckdb_aggregate_init_t,
        update: duckdb_aggregate_update_t,
        combine: duckdb_aggregate_combine_t,
        finalize: duckdb_aggregate_finalize_t,
    );
    pub fn duckdb_aggregate_function_set_destructor(
        function: duckdb_aggregate_function,
        destroy: duckdb_aggregate_destroy_t,
    );
    pub fn duckdb_register_aggregate_function(
        connection: duckdb_connection,
        function: duckdb_aggregate_function,
    ) -> duckdb_state;
    pub fn duckdb_aggregate_function_set_special_handling(function: duckdb_aggregate_function);
    pub fn duckdb_aggregate_function_set_extra_info(
        function: duckdb_aggregate_function,
        extra_info: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_aggregate_function_get_extra_info(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_aggregate_function_set_error(info: duckdb_function_info, message: *const c_char);
    pub fn duckdb_create_aggregate_function_set(
        name: *const c_char,
    ) -> duckdb_aggregate_function_set;
    pub fn duckdb_destroy_aggregate_function_set(set: *mut duckdb_aggregate_function_set);
    pub fn duckdb_add_aggregate_function_to_set(
        set: duckdb_aggregate_function_set,
        function: duckdb_aggregate_function,
    ) -> duckdb_state;
    pub fn duckdb_register_aggregate_function_set(
        connection: duckdb_connection,
        set: duckdb_aggregate_function_set,
    ) -> duckdb_state;

    pub fn duckdb_create_table_function() -> duckdb_table_function;
    pub fn duckdb_destroy_table_function(function: *mut duckdb_table_function);
    pub fn duckdb_table_function_set_name(function: duckdb_table_function, name: *const c_char);
    pub fn duckdb_table_function_add_parameter(
        function: duckdb_table_function,
        param_type: duckdb_logical_type,
    );
    pub fn duckdb_table_function_add_named_parameter(
        function: duckdb_table_function,
        name: *const c_char,
        param_type: duckdb_logical_type,
    );
    pub fn duckdb_table_function_set_extra_info(
        function: duckdb_table_function,
        extra_info: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_table_function_set_bind(
        function: duckdb_table_function,
        bind: duckdb_table_function_bind_t,
    );
    pub fn duckdb_table_function_set_init(
        function: duckdb_table_function,
        init: duckdb_table_function_init_t,
    );
    pub fn duckdb_table_function_set_local_init(
        function: duckdb_table_function,
        init: duckdb_table_function_init_t,
    );
    pub fn duckdb_table_function_set_function(
        function: duckdb_table_function,
        callback: duckdb_table_function_t,
    );
    pub fn duckdb_table_function_supports_projection_pushdown(
        function: duckdb_table_function,
        pushdown: bool,
    );
    pub fn duckdb_register_table_function(
        connection: duckdb_connection,
        function: duckdb_table_function,
    ) -> duckdb_state;

    pub fn duckdb_bind_get_extra_info(info: duckdb_bind_info) -> *mut c_void;
    pub fn duckdb_table_function_get_client_context(
        info: duckdb_bind_info,
        out_context: *mut duckdb_client_context,
    );
    pub fn duckdb_bind_add_result_column(
        info: duckdb_bind_info,
        name: *const c_char,
        logical_type: duckdb_logical_type,
    );
    pub fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t;
    pub fn duckdb_bind_get_parameter(info: duckdb_bind_info, index: idx_t) -> duckdb_value;
    pub fn duckdb_bind_get_named_parameter(
        info: duckdb_bind_info,
        name: *const c_char,
    ) -> duckdb_value;
    pub fn duckdb_bind_set_bind_data(
        info: duckdb_bind_info,
        bind_data: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_bind_set_cardinality(info: duckdb_bind_info, cardinality: idx_t, is_exact: bool);
    pub fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char);

    pub fn duckdb_init_get_extra_info(info: duckdb_init_info) -> *mut c_void;
    pub fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut c_void;
    pub fn duckdb_init_set_init_data(
        info: duckdb_init_info,
        init_data: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_init_get_column_count(info: duckdb_init_info) -> idx_t;
    pub fn duckdb_init_get_column_index(info: duckdb_init_info, column_index: idx_t) -> idx_t;
    pub fn duckdb_init_set_max_threads(info: duckdb_init_info, max_threads: idx_t);
    pub fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char);

    pub fn duckdb_function_get_extra_info(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_get_local_init_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_set_error(info: duckdb_function_info, error: *const c_char);

    pub fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t;
    pub fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t);
    pub fn duckdb_data_chunk_get_vector(chunk: duckdb_data_chunk, col_idx: idx_t) -> duckdb_vector;
    pub fn duckdb_vector_get_column_type(vector: duckdb_vector) -> duckdb_logical_type;
    pub fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut c_void;
    pub fn duckdb_vector_get_validity(vector: duckdb_vector) -> *mut u64;
    pub fn duckdb_vector_assign_string_element(
        vector: duckdb_vector,
        index: idx_t,
        value: *const c_char,
    );
    pub fn duckdb_vector_assign_string_element_len(
        vector: duckdb_vector,
        index: idx_t,
        value: *const c_char,
        len: idx_t,
    );
    pub fn duckdb_validity_set_row_validity(validity: *mut u64, row: idx_t, valid: bool);
    pub fn duckdb_validity_set_row_invalid(validity: *mut u64, row: idx_t);
    pub fn duckdb_validity_set_row_valid(validity: *mut u64, row: idx_t);
    pub fn duckdb_create_logical_type(kind: duckdb_type) -> duckdb_logical_type;
    pub fn duckdb_destroy_logical_type(logical_type: *mut duckdb_logical_type);
    pub fn duckdb_get_type_id(logical_type: duckdb_logical_type) -> duckdb_type;
    pub fn duckdb_validity_row_is_valid(validity: *mut u64, row: idx_t) -> bool;
    pub fn duckdb_string_is_inlined(string: duckdb_string_t) -> bool;
    pub fn duckdb_string_t_length(string: duckdb_string_t) -> u32;
    pub fn duckdb_string_t_data(string: *mut duckdb_string_t) -> *const c_char;

    pub fn duckdb_create_varchar(text: *const c_char) -> duckdb_value;
    pub fn duckdb_add_replacement_scan(
        db: duckdb_database,
        replacement: duckdb_replacement_callback_t,
        extra_data: *mut c_void,
        delete_callback: duckdb_delete_callback_t,
    );
    pub fn duckdb_replacement_scan_set_function_name(
        info: duckdb_replacement_scan_info,
        function_name: *const c_char,
    );
    pub fn duckdb_replacement_scan_add_parameter(
        info: duckdb_replacement_scan_info,
        parameter: duckdb_value,
    );

    pub fn duckdb_column_logical_type(result: *mut duckdb_result, col: idx_t) -> duckdb_logical_type;
    pub fn duckdb_create_cast_function() -> duckdb_cast_function;
    pub fn duckdb_destroy_cast_function(cast_function: *mut duckdb_cast_function);
    pub fn duckdb_cast_function_set_source_type(
        cast_function: duckdb_cast_function,
        source_type: duckdb_logical_type,
    );
    pub fn duckdb_cast_function_set_target_type(
        cast_function: duckdb_cast_function,
        target_type: duckdb_logical_type,
    );
    pub fn duckdb_cast_function_set_implicit_cast_cost(
        cast_function: duckdb_cast_function,
        cost: i64,
    );
    pub fn duckdb_cast_function_set_function(
        cast_function: duckdb_cast_function,
        function: duckdb_cast_function_t,
    );
    pub fn duckdb_cast_function_set_extra_info(
        cast_function: duckdb_cast_function,
        extra_info: *mut c_void,
        destroy: duckdb_delete_callback_t,
    );
    pub fn duckdb_cast_function_get_extra_info(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_register_cast_function(
        con: duckdb_connection,
        cast_function: duckdb_cast_function,
    ) -> duckdb_state;
}
