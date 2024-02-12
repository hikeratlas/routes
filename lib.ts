import type { Feature, NodeMap } from './types';
const fs = require('node:fs');

export const load = (filename: string): Feature[] => {
	const lines = fs.readFileSync(filename, 'utf-8')
		.split('\n')
		.filter((x: string) => x)
		.map((x: string) => JSON.parse(x));
	//console.log(lines);
	console.log(lines.length);
	return lines;
}

export const ll = (coord: [number, number]): string => coord.join(',');

export const buildIndex = (features: Feature[]): NodeMap => {
	const map: NodeMap = {};

	const index = (properties: Feature['properties'], coordinates: [number, number], neighbour?: [number, number]) => {
		const key = ll(coordinates);

		if (!map[key])
			map[key] = {
				nodes: [],
				edges: []
			};

		map[key].nodes.push(properties);

		if (neighbour)
			map[key].edges.push(neighbour);
	};

	for (const feature of features) {
		if (feature.geometry.type === 'Point') {
			index(feature.properties, feature.geometry.coordinates);
		} else if (feature.geometry.type === 'LineString') {
			const coords = feature.geometry.coordinates;
			for (let i = 0; i < coords.length; i++)
				index(feature.properties, coords[i], coords[i + 1]);
		} else if (feature.geometry.type === 'MultiLineString') {
		} else if (feature.geometry.type === 'MultiPolygon') {
			// Just index the outer rings, e.g. for parking lots
			for (const poly of feature.geometry.coordinates) {
				const coords = poly[0];
				// The last coord is the same as the first coord, so skip it.
				for (let i = 0; i < coords.length - 1; i++)
					index(feature.properties, coords[i], coords[i + 1]);
			}
		}
	}

	return map;
}

export const isParkingLot = (node: Feature['properties']): boolean => {
	return node['amenity'] === 'parking';
}

export const isFootPath = (node: Feature['properties']): boolean => {
	return node['highway'] === 'footway';
}
