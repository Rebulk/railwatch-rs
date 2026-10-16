# Dumps Railwatch::Ingest::Mapper.row_for (gem 0.9.0, the cloud's) for each
# record in $stdin (NDJSON), as JSON: {table, row:[[col, [kind, value]]], truncations} or {error}.
out = []
$stdin.each_line do |line|
  rec = JSON.parse(line)
  truncations = []
  begin
    if rec.is_a?(Hash) && rec["t"] == "user"
      p = Railwatch::Ingest::Mapper.person_record(rec)
      out << {"person" => p.except("timestamp")}
      next
    end
    pair = Railwatch::Ingest::Mapper.row_for(rec, truncations: truncations)
    if pair.nil?
      out << {"unknown" => true}
    else
      klass, attrs = pair
      row = attrs.map do |k, v|
        enc = case v
              when nil then ["null", nil]
              when Integer then ["int", v.to_s]
              when Float then ["float", [v].pack("G").unpack1("H*")]
              when String then v.encoding == Encoding::BINARY ? ["blob", v.unpack1("H*")] : ["text", v.unpack1("H*")]
              else ["other", v.inspect]
              end
        [k.to_s, enc]
      end
      out << {"table" => klass.table_name, "row" => row, "truncations" => truncations}
    end
  rescue ArgumentError, TypeError, KeyError, NoMethodError, RangeError, JSON::GeneratorError => e
    out << {"error" => "#{e.class}: #{e.message}"[0, 200]}
  rescue StandardError => e
    out << {"error" => "invalid record", "class" => e.class.name, "message" => e.message[0, 200]}
  end
end
File.write("/tmp/rowfor_out.json", JSON.generate(out))
puts "ROWFOR #{out.size}"
