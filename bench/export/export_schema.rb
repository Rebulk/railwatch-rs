# Run inside the rw-baseline container: dumps the telemetry DDL and the column
# metadata Railwatch::Ingest::Mapper casts with, so the Rust server is generated
# from the exact schema the gem migrates, not a hand port.
env = Environment.first
out = { "ddl" => [], "columns" => {}, "text_limits" => Railwatch::Ingest::Mapper::TEXT_LIMITS,
        "rails_version" => Rails.version, "railwatch_version" => Railwatch::VERSION }
env.with_telemetry do
  conn = TelemetryRecord.connection
  out["ddl"] = conn.select_rows("SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid")
  out["schema_migrations"] = conn.select_values("SELECT version FROM schema_migrations ORDER BY version")
  out["pragmas"] = %w[auto_vacuum journal_mode page_size].to_h { |p| [p, conn.select_value("PRAGMA #{p}")] }
  Telemetry.constants.map { |c| Telemetry.const_get(c) }
    .select { |k| k.is_a?(Class) && k < TelemetryRecord && !k.abstract_class? && conn.table_exists?(k.table_name) }
    .sort_by(&:table_name).each do |k|
      out["columns"][k.table_name] = k.columns.map { |c| [c.name, c.type.to_s, c.limit, c.null, c.default] }
    end
end
File.write("/tmp/schema_export.json", JSON.pretty_generate(out))
puts "EXPORT tables=#{out['columns'].size} ddl=#{out['ddl'].size}"
