const fs = require('node:fs');

const load = () => {
	const lines = fs.readFileSync('../basemap/trails.geojsonl', 'utf-8').split('\n').filter(x => x).map(x => JSON.parse(x));
	//console.log(lines);
	console.log(lines.length);
	return lines;
}

const lines = load();
