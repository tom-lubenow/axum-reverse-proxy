#!/usr/bin/env python3
import os
import glob
import markdown

def read_template():
    with open('benchmarks/templates/index.html', 'r') as f:
        return f.read()

def read_latest_results():
    result_files = glob.glob('benchmarks/results/*.md')
    if not result_files:
        return "No benchmark results found."
    
    latest_file = max(result_files, key=os.path.getctime)
    with open(latest_file, 'r') as f:
        content = f.read()
    
    return markdown.markdown(content)

def generate_report():
    template = read_template()
    results = read_latest_results()
    
    # Replace the placeholder with the actual results
    html = template.replace('<!-- RESULTS_PLACEHOLDER -->', results)
    
    # Write the final report
    os.makedirs('benchmark_results', exist_ok=True)
    with open('benchmark_results/index.html', 'w') as f:
        f.write(html) 