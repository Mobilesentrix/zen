// Parity harness: runs the Go module's Lookup-step conversion (rulestore.TransformDatabaseNodesRouted,
// copied verbatim next to this file by run.sh) over cases.jsonl and writes expected.jsonl, one line per case:
// {"rule": <converted rule>} or {"error": "<message>"}.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
)

type parityCase struct {
	Routing string          `json:"routing"`
	Content json.RawMessage `json:"content"`
	Rule    json.RawMessage `json:"rule"`
}

func main() {
	in, err := os.Open(os.Args[1])
	if err != nil {
		panic(err)
	}
	defer in.Close()
	out := bufio.NewWriter(os.Stdout)
	defer out.Flush()

	scanner := bufio.NewScanner(in)
	scanner.Buffer(make([]byte, 1<<20), 1<<24)
	for scanner.Scan() {
		var c parityCase
		if err := json.Unmarshal(scanner.Bytes(), &c); err != nil {
			panic(err)
		}
		raw := c.Rule
		if raw == nil {
			raw = []byte(fmt.Sprintf(`{"nodes":[{"id":"db","name":"Lookup","type":"databaseNode","content":%s}],"edges":[]}`, c.Content))
		}
		var data map[string]any
		if err := json.Unmarshal(raw, &data); err != nil {
			panic(err)
		}
		line := map[string]any{}
		if err := TransformDatabaseNodesRouted(data, c.Routing == "instance"); err != nil {
			line["error"] = err.Error()
		} else {
			line["rule"] = data
		}
		encoded, err := json.Marshal(line)
		if err != nil {
			panic(err)
		}
		out.Write(encoded)
		out.WriteByte('\n')
	}
	if err := scanner.Err(); err != nil {
		panic(err)
	}
}
