# procstat -v: a process's map (its program's segments, the stack, the
# shared page, and anonymous, shared, file, and guard mappings).
helper mapper vm
show procstat -v $mapper
